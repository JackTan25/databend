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

use super::cost::CostContext;
use super::format::display_memo;
use super::Memo;
use crate::optimizer::cascades::CascadesOptimizer;
use crate::optimizer::distributed::optimize_distributed_query;
use crate::optimizer::hyper_dp::DPhpy;
use crate::optimizer::runtime_filter::try_add_runtime_filter_nodes;
use crate::optimizer::util::contains_local_table_scan;
use crate::optimizer::HeuristicOptimizer;
use crate::optimizer::SExpr;
use crate::plans::CopyIntoTablePlan;
use crate::plans::CopyPlan;
use crate::plans::Plan;
use crate::BindContext;
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
            s_expr: Box::new(optimize_query(
                ctx,
                opt_ctx,
                metadata.clone(),
                bind_context.clone(),
                *s_expr,
            )?),
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
                    ref bind_context,
                    ..
                } = plan
                {
                    let (memo, cost_map) = get_optimized_memo(
                        ctx,
                        *s_expr.clone(),
                        metadata.clone(),
                        bind_context.clone(),
                    )?;
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
        Plan::Copy(v) => {
            Ok(Plan::Copy(Box::new(match *v {
                CopyPlan::IntoStage {
                    stage,
                    path,
                    validation_mode,
                    from,
                } => {
                    CopyPlan::IntoStage {
                        stage,
                        path,
                        validation_mode,
                        // Make sure the subquery has been optimized.
                        from: Box::new(optimize(ctx, opt_ctx, *from)?),
                    }
                }
                CopyPlan::NoFileToCopy => *v,
                CopyPlan::IntoTable(into_table) => match into_table.query {
                    Some(_) => CopyPlan::IntoTable(into_table),
                    None => CopyPlan::IntoTable(CopyIntoTablePlan {
                        catalog_name: into_table.catalog_name,
                        database_name: into_table.database_name,
                        table_name: into_table.table_name,

                        required_values_schema: into_table.required_values_schema,
                        values_consts: into_table.values_consts,
                        required_source_schema: into_table.required_source_schema,

                        write_mode: into_table.write_mode,
                        validation_mode: into_table.validation_mode,
                        force: into_table.force,

                        stage_table_info: into_table.stage_table_info,
                        query: into_table.query,

                        enable_distributed: opt_ctx.config.enable_distributed_optimization,
                    }),
                },
            })))
        }
        // Passthrough statements
        _ => Ok(plan),
    }
}

pub fn optimize_query(
    ctx: Arc<dyn TableContext>,
    opt_ctx: Arc<OptimizerContext>,
    metadata: MetadataRef,
    bind_context: Box<BindContext>,
    s_expr: SExpr,
) -> Result<SExpr> {
    let contains_local_table_scan = contains_local_table_scan(&s_expr, &metadata);

    let heuristic =
        HeuristicOptimizer::new(ctx.get_function_context()?, bind_context, metadata.clone());
    let mut result = heuristic.optimize(s_expr)?;
    if ctx.get_settings().get_enable_dphyp()? {
        let (dp_res, optimized) =
            DPhpy::new(ctx.clone(), metadata.clone()).optimize(Arc::new(result))?;
        result = (*dp_res).clone();
        let mut cascades = CascadesOptimizer::create(ctx.clone(), metadata, optimized)?;
        result = cascades.optimize(result)?;
    } else {
        let mut cascades = CascadesOptimizer::create(ctx.clone(), metadata, false)?;
        result = cascades.optimize(result)?;
    }
    // So far, we don't have ability to execute distributed query
    // with reading data from local tales(e.g. system tables).
    let enable_distributed_query =
        opt_ctx.config.enable_distributed_optimization && !contains_local_table_scan;
    // Add runtime filter related nodes after cbo
    // Because cbo may change join order and we don't want to
    // break optimizer due to new added nodes by runtime filter.
    // Currently, we only support standalone.
    if !enable_distributed_query && ctx.get_settings().get_runtime_filter()? {
        result = try_add_runtime_filter_nodes(&result)?;
    }
    if enable_distributed_query {
        result = optimize_distributed_query(ctx.clone(), &result)?;
    }

    Ok(result)
}

// TODO(leiysky): reuse the optimization logic with `optimize_query`
fn get_optimized_memo(
    ctx: Arc<dyn TableContext>,
    s_expr: SExpr,
    metadata: MetadataRef,
    bind_context: Box<BindContext>,
) -> Result<(Memo, HashMap<IndexType, CostContext>)> {
    let heuristic =
        HeuristicOptimizer::new(ctx.get_function_context()?, bind_context, metadata.clone());
    let result = heuristic.optimize(s_expr)?;

    let mut cascades = CascadesOptimizer::create(ctx, metadata, false)?;
    cascades.optimize(result)?;
    Ok((cascades.memo, cascades.best_cost_map))
}
