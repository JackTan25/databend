// Copyright 2021 Datafuse Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::sync::Arc;

use common_ast::ast::ExplainKind;
use common_catalog::table_context::TableContext;
use common_exception::ErrorCode;
use common_exception::Result;
use log::info;

use super::cost::CostContext;
use super::distributed::MergeSourceOptimizer;
use super::format::display_memo;
use super::rule::TransformResult;
use super::Memo;
use super::RuleFactory;
use crate::optimizer::cascades::CascadesOptimizer;
use crate::optimizer::distributed::optimize_distributed_query;
use crate::optimizer::hyper_dp::DPhpy;
use crate::optimizer::util::contains_local_table_scan;
use crate::optimizer::HeuristicOptimizer;
use crate::optimizer::RuleID;
use crate::optimizer::SExpr;
use crate::optimizer::DEFAULT_REWRITE_RULES;
use crate::optimizer::RESIDUAL_RULES;
use crate::plans::CopyIntoLocationPlan;
use crate::plans::MergeInto;
use crate::plans::Plan;
use crate::plans::RelOperator;
use crate::IndexType;
use crate::MetadataRef;

#[derive(Debug, Clone, Default)]
pub struct OptimizerConfig {
    pub enable_distributed_optimization: bool,
}

#[derive(Debug)]
pub struct OptimizerContext {
    pub config: OptimizerConfig,
}

impl OptimizerContext {
    pub fn new(config: OptimizerConfig) -> Self {
        Self { config }
    }
}

#[minitrace::trace]
pub fn optimize(
    ctx: Arc<dyn TableContext>,
    opt_ctx: Arc<OptimizerContext>,
    plan: Plan,
) -> Result<Plan> {
    match plan {
        Plan::Query {
            s_expr,
            bind_context,
            metadata,
            rewrite_kind,
            formatted_ast,
            ignore_result,
        } => Ok(Plan::Query {
            s_expr: Box::new(optimize_query(ctx, opt_ctx, metadata.clone(), *s_expr)?),
            bind_context,
            metadata,
            rewrite_kind,
            formatted_ast,
            ignore_result,
        }),
        Plan::Explain { kind, plan } => match kind {
            ExplainKind::Raw | ExplainKind::Ast(_) | ExplainKind::Syntax(_) => {
                Ok(Plan::Explain { kind, plan })
            }
            ExplainKind::Memo(_) => {
                if let box Plan::Query {
                    ref s_expr,
                    ref metadata,
                    ..
                } = plan
                {
                    let (memo, cost_map) =
                        get_optimized_memo(ctx, *s_expr.clone(), metadata.clone())?;
                    Ok(Plan::Explain {
                        kind: ExplainKind::Memo(display_memo(&memo, &cost_map)?),
                        plan,
                    })
                } else {
                    Err(ErrorCode::BadArguments(
                        "Cannot use EXPLAIN MEMO with a non-query statement",
                    ))
                }
            }
            _ => Ok(Plan::Explain {
                kind,
                plan: Box::new(optimize(ctx, opt_ctx, *plan)?),
            }),
        },
        Plan::ExplainAnalyze { plan } => Ok(Plan::ExplainAnalyze {
            plan: Box::new(optimize(ctx, opt_ctx, *plan)?),
        }),
        Plan::CopyIntoLocation(CopyIntoLocationPlan { stage, path, from }) => {
            Ok(Plan::CopyIntoLocation(CopyIntoLocationPlan {
                stage,
                path,
                from: Box::new(optimize(ctx, opt_ctx, *from)?),
            }))
        }
        Plan::CopyIntoTable(mut plan) if !plan.no_file_to_copy => {
            plan.enable_distributed = opt_ctx.config.enable_distributed_optimization
                && ctx.get_settings().get_enable_distributed_copy()?;
            info!(
                "after optimization enable_distributed_copy? : {}",
                plan.enable_distributed
            );
            Ok(Plan::CopyIntoTable(plan))
        }
        Plan::MergeInto(plan) => {
            // optimize source :fix issue #13733
            // reason: if there is subquery,windowfunc exprs etc. see
            // src/planner/semantic/lowering.rs `as_raw_expr()`, we will
            // get dummy index. So we need to use optimizer to solve this.
            let mut right_source = optimize_query(
                ctx.clone(),
                opt_ctx.clone(),
                plan.meta_data.clone(),
                plan.input.child(1)?.clone(),
            )?;

            // if it's not distributed execution, we should reserve
            // exchange to merge source data.
            if opt_ctx.config.enable_distributed_optimization
                && ctx.get_settings().get_enable_distributed_merge_into()?
            {
                // we need to remove exchange of right_source, because it's
                // not an end query.
                if let RelOperator::Exchange(_) = right_source.plan.as_ref() {
                    right_source = right_source.child(0)?.clone();
                }
            }
            // replace right source
            let mut join_sexpr = plan.input.clone();
            join_sexpr = Box::new(join_sexpr.replace_children(vec![
                Arc::new(join_sexpr.child(0)?.clone()),
                Arc::new(right_source),
            ]));

            // before, we think source table is always the small table.
            // 1. for matched only, we use inner join
            // 2. for insert only, we use right anti join
            // 3. for full merge into, we use right outer join
            // for now, let's import the statistic info to determine left join or right join
            // we just do optimization for the top join (target and source),won't do recursive optimization.
            let rule = RuleFactory::create_rule(RuleID::CommuteJoin, plan.meta_data.clone())?;
            let mut state = TransformResult::new();
            // we will reorder the join order according to the cardinality of target and source.
            rule.apply(&join_sexpr, &mut state)?;
            assert_eq!(state.results().len(), 1);
            // we need to check whether we do swap left and right.
            let old_left = join_sexpr.child(0)?;
            let new_left = state.results()[0].child(0)?;
            let change_join_order = old_left == new_left;
            join_sexpr = Box::new(state.results()[0].clone());
            // try to optimize distributed join
            if opt_ctx.config.enable_distributed_optimization
                && ctx.get_settings().get_enable_distributed_merge_into()?
            {
                // input is a Join_SExpr
                let merge_into_join_sexpr = optimize_distributed_query(ctx.clone(), &join_sexpr)?;
                // after optimize source, we need to add
                let merge_source_optimizer = MergeSourceOptimizer::create();
                let (optimized_distributed_merge_into_join_sexpr, distributed) =
                    if !merge_into_join_sexpr
                        .match_pattern(&merge_source_optimizer.merge_source_pattern)
                    {
                        (merge_into_join_sexpr.clone(), false)
                    } else {
                        (
                            merge_source_optimizer
                                .optimize(&merge_into_join_sexpr, change_join_order)?,
                            true,
                        )
                    };

                Ok(Plan::MergeInto(Box::new(MergeInto {
                    input: Box::new(optimized_distributed_merge_into_join_sexpr),
                    distributed,
                    change_join_order,
                    ..*plan
                })))
            } else {
                Ok(Plan::MergeInto(Box::new(MergeInto {
                    input: join_sexpr,
                    change_join_order,
                    ..*plan
                })))
            }
        }
        // Passthrough statements.
        _ => Ok(plan),
    }
}

pub fn optimize_query(
    ctx: Arc<dyn TableContext>,
    opt_ctx: Arc<OptimizerContext>,
    metadata: MetadataRef,
    s_expr: SExpr,
) -> Result<SExpr> {
    let contains_local_table_scan = contains_local_table_scan(&s_expr, &metadata);

    let heuristic = HeuristicOptimizer::new(ctx.get_function_context()?, metadata.clone());
    let mut result = heuristic.pre_optimize(s_expr)?;
    result = heuristic.optimize_expression(&result, &DEFAULT_REWRITE_RULES)?;
    let mut dphyp_optimized = false;
    if ctx.get_settings().get_enable_dphyp()?
        && unsafe { !ctx.get_settings().get_disable_join_reorder()? }
    {
        let (dp_res, optimized) =
            DPhpy::new(ctx.clone(), metadata.clone()).optimize(Arc::new(result.clone()))?;
        if optimized {
            result = (*dp_res).clone();
            dphyp_optimized = true;
        }
    }
    let mut cascades = CascadesOptimizer::create(ctx.clone(), metadata, dphyp_optimized)?;
    result = cascades.optimize(result)?;
    // So far, we don't have ability to execute distributed query
    // with reading data from local tales(e.g. system tables).
    let enable_distributed_query =
        opt_ctx.config.enable_distributed_optimization && !contains_local_table_scan;
    if enable_distributed_query {
        result = optimize_distributed_query(ctx.clone(), &result)?;
    }
    if unsafe { ctx.get_settings().get_disable_join_reorder()? } {
        return heuristic.optimize_expression(&result, &[RuleID::EliminateEvalScalar]);
    }
    let result = heuristic.optimize_expression(&result, &RESIDUAL_RULES)?;
    Ok(result)
}

// TODO(leiysky): reuse the optimization logic with `optimize_query`
fn get_optimized_memo(
    ctx: Arc<dyn TableContext>,
    s_expr: SExpr,
    metadata: MetadataRef,
) -> Result<(Memo, HashMap<IndexType, CostContext>)> {
    let heuristic = HeuristicOptimizer::new(ctx.get_function_context()?, metadata.clone());
    let result = heuristic.optimize(s_expr, &DEFAULT_REWRITE_RULES)?;

    let mut cascades = CascadesOptimizer::create(ctx, metadata, false)?;
    cascades.optimize(result)?;
    Ok((cascades.memo, cascades.best_cost_map))
}
