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

use std::convert::TryFrom;
use std::sync::Arc;

use async_channel::Receiver;
use common_catalog::table::AppendMode;
use common_exception::ErrorCode;
use common_exception::Result;
use common_expression::type_check::check_function;
use common_expression::types::DataType;
use common_expression::types::NumberDataType;
use common_expression::with_hash_method;
use common_expression::with_mappedhash_method;
use common_expression::with_number_mapped_type;
use common_expression::DataBlock;
use common_expression::DataSchemaRef;
use common_expression::FunctionContext;
use common_expression::HashMethodKind;
use common_expression::RemoteExpr;
use common_expression::SortColumnDescription;
use common_functions::aggregates::AggregateFunctionFactory;
use common_functions::aggregates::AggregateFunctionRef;
use common_functions::BUILTIN_FUNCTIONS;
use common_pipeline_core::pipe::Pipe;
use common_pipeline_core::pipe::PipeItem;
use common_pipeline_core::processors::port::InputPort;
use common_pipeline_core::processors::processor::ProcessorPtr;
use common_pipeline_core::processors::Processor;
use common_pipeline_sinks::EmptySink;
use common_pipeline_sinks::Sinker;
use common_pipeline_sinks::UnionReceiveSink;
use common_pipeline_transforms::processors::transforms::build_full_sort_pipeline;
use common_pipeline_transforms::processors::ProfileWrapper;
use common_profile::SharedProcessorProfiles;
use common_sql::evaluator::BlockOperator;
use common_sql::evaluator::CompoundBlockOperator;
use common_sql::executor::AggregateExpand;
use common_sql::executor::AggregateFinal;
use common_sql::executor::AggregateFunctionDesc;
use common_sql::executor::AggregatePartial;
use common_sql::executor::DeleteFinal;
use common_sql::executor::DeletePartial;
use common_sql::executor::DistributedCopyIntoTable;
use common_sql::executor::DistributedInsertSelect;
use common_sql::executor::EvalScalar;
use common_sql::executor::ExchangeSink;
use common_sql::executor::ExchangeSource;
use common_sql::executor::Filter;
use common_sql::executor::HashJoin;
use common_sql::executor::Limit;
use common_sql::executor::PhysicalPlan;
use common_sql::executor::Project;
use common_sql::executor::ProjectSet;
use common_sql::executor::RangeJoin;
use common_sql::executor::RowFetch;
use common_sql::executor::RuntimeFilterSource;
use common_sql::executor::Sort;
use common_sql::executor::TableScan;
use common_sql::executor::UnionAll;
use common_sql::executor::Window;
use common_sql::plans::JoinType;
use common_sql::ColumnBinding;
use common_sql::IndexType;
use common_storage::DataOperator;
use common_storages_factory::Table;
use common_storages_fuse::operations::build_row_fetcher_pipeline;
use common_storages_fuse::operations::FillInternalColumnProcessor;
use common_storages_fuse::operations::SerializeDataTransform;
use common_storages_fuse::FuseTable;
use common_storages_stage::StageTable;
use petgraph::matrix_graph::Zero;

use super::processors::transforms::FrameBound;
use super::processors::transforms::WindowFunctionInfo;
use super::processors::TransformExpandGroupingSets;
use crate::api::DefaultExchangeInjector;
use crate::api::ExchangeInjector;
use crate::interpreters::chain_append_data_pipeline;
use crate::interpreters::fill_missing_columns;
use crate::interpreters::PlanParam;
use crate::pipelines::processors::transforms::build_partition_bucket;
use crate::pipelines::processors::transforms::AggregateInjector;
use crate::pipelines::processors::transforms::FinalSingleStateAggregator;
use crate::pipelines::processors::transforms::HashJoinDesc;
use crate::pipelines::processors::transforms::PartialSingleStateAggregator;
use crate::pipelines::processors::transforms::RangeJoinState;
use crate::pipelines::processors::transforms::RuntimeFilterState;
use crate::pipelines::processors::transforms::TransformAggregateSpillWriter;
use crate::pipelines::processors::transforms::TransformGroupBySpillWriter;
use crate::pipelines::processors::transforms::TransformMarkJoin;
use crate::pipelines::processors::transforms::TransformMergeBlock;
use crate::pipelines::processors::transforms::TransformPartialAggregate;
use crate::pipelines::processors::transforms::TransformPartialGroupBy;
use crate::pipelines::processors::transforms::TransformRangeJoinLeft;
use crate::pipelines::processors::transforms::TransformRangeJoinRight;
use crate::pipelines::processors::transforms::TransformWindow;
use crate::pipelines::processors::AggregatorParams;
use crate::pipelines::processors::JoinHashTable;
use crate::pipelines::processors::MarkJoinCompactor;
use crate::pipelines::processors::SinkRuntimeFilterSource;
use crate::pipelines::processors::TransformCastSchema;
use crate::pipelines::processors::TransformHashJoinBuild;
use crate::pipelines::processors::TransformHashJoinProbe;
use crate::pipelines::processors::TransformLimit;
use crate::pipelines::processors::TransformRuntimeFilter;
use crate::pipelines::Pipeline;
use crate::pipelines::PipelineBuildResult;
use crate::sessions::QueryContext;
use crate::sessions::TableContext;

pub struct PipelineBuilder {
    ctx: Arc<QueryContext>,

    main_pipeline: Pipeline,
    pub pipelines: Vec<Pipeline>,

    // Used in runtime filter source
    pub join_state: Option<Arc<JoinHashTable>>,
    // record the index of join build side pipeline in `pipelines`
    pub index: Option<usize>,

    enable_profiling: bool,
    prof_span_set: SharedProcessorProfiles,
    exchange_injector: Arc<dyn ExchangeInjector>,
}

impl PipelineBuilder {
    pub fn create(
        ctx: Arc<QueryContext>,
        enable_profiling: bool,
        prof_span_set: SharedProcessorProfiles,
    ) -> PipelineBuilder {
        PipelineBuilder {
            enable_profiling,
            ctx,
            pipelines: vec![],
            join_state: None,
            main_pipeline: Pipeline::create(),
            prof_span_set,
            exchange_injector: DefaultExchangeInjector::create(),
            index: None,
        }
    }

    pub fn finalize(mut self, plan: &PhysicalPlan) -> Result<PipelineBuildResult> {
        self.build_pipeline(plan)?;

        for source_pipeline in &self.pipelines {
            if !source_pipeline.is_complete_pipeline()? {
                return Err(ErrorCode::Internal(
                    "Source pipeline must be complete pipeline.",
                ));
            }
        }

        Ok(PipelineBuildResult {
            main_pipeline: self.main_pipeline,
            sources_pipelines: self.pipelines,
            prof_span_set: self.prof_span_set,
            exchange_injector: self.exchange_injector,
        })
    }

    fn build_pipeline(&mut self, plan: &PhysicalPlan) -> Result<()> {
        match plan {
            PhysicalPlan::TableScan(scan) => self.build_table_scan(scan),
            PhysicalPlan::Filter(filter) => self.build_filter(filter),
            PhysicalPlan::Project(project) => self.build_project(project),
            PhysicalPlan::EvalScalar(eval_scalar) => self.build_eval_scalar(eval_scalar),
            PhysicalPlan::AggregateExpand(aggregate) => self.build_aggregate_expand(aggregate),
            PhysicalPlan::AggregatePartial(aggregate) => self.build_aggregate_partial(aggregate),
            PhysicalPlan::AggregateFinal(aggregate) => self.build_aggregate_final(aggregate),
            PhysicalPlan::Window(window) => self.build_window(window),
            PhysicalPlan::Sort(sort) => self.build_sort(sort),
            PhysicalPlan::Limit(limit) => self.build_limit(limit),
            PhysicalPlan::RowFetch(row_fetch) => self.build_row_fetch(row_fetch),
            PhysicalPlan::HashJoin(join) => self.build_join(join),
            PhysicalPlan::ExchangeSink(sink) => self.build_exchange_sink(sink),
            PhysicalPlan::ExchangeSource(source) => self.build_exchange_source(source),
            PhysicalPlan::UnionAll(union_all) => self.build_union_all(union_all),
            PhysicalPlan::DistributedInsertSelect(insert_select) => {
                self.build_distributed_insert_select(insert_select)
            }
            PhysicalPlan::ProjectSet(project_set) => self.build_project_set(project_set),
            PhysicalPlan::Exchange(_) => Err(ErrorCode::Internal(
                "Invalid physical plan with PhysicalPlan::Exchange",
            )),
            PhysicalPlan::RuntimeFilterSource(runtime_filter_source) => {
                self.build_runtime_filter_source(runtime_filter_source)
            }
            PhysicalPlan::DeletePartial(delete) => self.build_delete_partial(delete),
            PhysicalPlan::DeleteFinal(delete) => self.build_delete_final(delete),
            PhysicalPlan::RangeJoin(range_join) => self.build_range_join(range_join),
            PhysicalPlan::DistributedCopyIntoTable(distributed_plan) => {
                self.build_distributed_copy_into_table(distributed_plan)
            }
        }
    }

    fn build_distributed_copy_into_table(
        &mut self,
        distributed_plan: &DistributedCopyIntoTable,
    ) -> Result<()> {
        let catalog = self.ctx.get_catalog(&distributed_plan.catalog_name)?;
        let to_table = catalog.get_table_by_info(&distributed_plan.table_info)?;
        let stage_table_info = distributed_plan.stage_table_info.clone();
        let stage_table = StageTable::try_create(stage_table_info)?;
        stage_table.set_block_thresholds(distributed_plan.thresholds);
        let ctx = self.ctx.clone();
        let table_ctx: Arc<dyn TableContext> = ctx.clone();
        stage_table.read_data(table_ctx, &distributed_plan.source, &mut self.main_pipeline)?;
        // append data
        chain_append_data_pipeline(
            &mut self.main_pipeline,
            distributed_plan.required_source_schema.clone(),
            PlanParam::DistributedCopyIntoTable(distributed_plan.clone()),
            ctx,
            to_table,
            distributed_plan.files.clone(),
            false,
        )?;
        Ok(())
    }

    /// The flow of Pipeline is as follows:
    ///
    /// +---------------+      +-----------------------+
    /// |MutationSource1| ---> |SerializeDataTransform1|   
    /// +---------------+      +-----------------------+               
    /// |     ...       | ---> |          ...          |
    /// +---------------+      +-----------------------+               
    /// |MutationSourceN| ---> |SerializeDataTransformN|   
    /// +---------------+      +-----------------------+
    fn build_delete_partial(&mut self, delete: &DeletePartial) -> Result<()> {
        let table =
            self.ctx
                .build_table_by_table_info(&delete.catalog_name, &delete.table_info, None)?;
        let table = FuseTable::try_from_table(table.as_ref())?;
        table.add_deletion_source(
            self.ctx.clone(),
            &delete.filter,
            delete.col_indices.clone(),
            delete.query_row_id_col,
            &mut self.main_pipeline,
            delete.parts.clone(),
        )?;
        let cluster_stats_gen =
            table.get_cluster_stats_gen(self.ctx.clone(), 0, table.get_block_thresholds())?;
        self.main_pipeline.add_transform(|input, output| {
            SerializeDataTransform::try_create(
                self.ctx.clone(),
                input,
                output,
                table,
                cluster_stats_gen.clone(),
            )
        })?;
        Ok(())
    }

    /// The flow of Pipeline is as follows:
    ///
    /// +-----------------------+      +----------+
    /// |TableMutationAggregator| ---> |CommitSink|
    /// +-----------------------+      +----------+
    fn build_delete_final(&mut self, delete: &DeleteFinal) -> Result<()> {
        self.build_pipeline(&delete.input)?;
        let table =
            self.ctx
                .build_table_by_table_info(&delete.catalog_name, &delete.table_info, None)?;
        let table = FuseTable::try_from_table(table.as_ref())?;
        let ctx: Arc<dyn TableContext> = self.ctx.clone();
        table.chain_mutation_pipes(
            &ctx,
            &mut self.main_pipeline,
            Arc::new(delete.snapshot.clone()),
        )?;
        Ok(())
    }

    fn build_range_join(&mut self, range_join: &RangeJoin) -> Result<()> {
        let state = Arc::new(RangeJoinState::new(self.ctx.clone(), range_join));
        self.expand_right_side_pipeline(range_join, state.clone())?;
        self.build_left_side(range_join, state)
    }

    fn build_left_side(
        &mut self,
        range_join: &RangeJoin,
        state: Arc<RangeJoinState>,
    ) -> Result<()> {
        self.build_pipeline(&range_join.left)?;
        let max_threads = self.ctx.get_settings().get_max_threads()? as usize;
        self.main_pipeline.try_resize(max_threads)?;
        self.main_pipeline.add_transform(|input, output| {
            let transform = TransformRangeJoinLeft::create(input, output, state.clone());
            if self.enable_profiling {
                Ok(ProcessorPtr::create(ProfileWrapper::create(
                    transform,
                    range_join.plan_id,
                    self.prof_span_set.clone(),
                )))
            } else {
                Ok(ProcessorPtr::create(transform))
            }
        })?;
        Ok(())
    }

    fn expand_right_side_pipeline(
        &mut self,
        range_join: &RangeJoin,
        state: Arc<RangeJoinState>,
    ) -> Result<()> {
        let right_side_context = QueryContext::create_from(self.ctx.clone());
        let right_side_builder = PipelineBuilder::create(
            right_side_context,
            self.enable_profiling,
            self.prof_span_set.clone(),
        );
        let mut right_res = right_side_builder.finalize(&range_join.right)?;
        right_res.main_pipeline.add_sink(|input| {
            let transform = Sinker::<TransformRangeJoinRight>::create(
                input,
                TransformRangeJoinRight::create(state.clone()),
            );
            if self.enable_profiling {
                Ok(ProcessorPtr::create(ProfileWrapper::create(
                    transform,
                    range_join.plan_id,
                    self.prof_span_set.clone(),
                )))
            } else {
                Ok(ProcessorPtr::create(transform))
            }
        })?;
        self.pipelines.push(right_res.main_pipeline);
        self.pipelines
            .extend(right_res.sources_pipelines.into_iter());
        Ok(())
    }

    fn build_join(&mut self, join: &HashJoin) -> Result<()> {
        let state = self.build_join_state(join)?;
        self.expand_build_side_pipeline(&join.build, join, state.clone())?;
        self.build_join_probe(join, state)
    }

    fn build_join_state(&mut self, join: &HashJoin) -> Result<Arc<JoinHashTable>> {
        JoinHashTable::create_join_state(
            self.ctx.clone(),
            &join.build_keys,
            join.build.output_schema()?,
            join.probe.output_schema()?,
            HashJoinDesc::create(join)?,
        )
    }

    fn expand_build_side_pipeline(
        &mut self,
        build: &PhysicalPlan,
        hash_join_plan: &HashJoin,
        join_state: Arc<JoinHashTable>,
    ) -> Result<()> {
        let build_side_context = QueryContext::create_from(self.ctx.clone());
        let build_side_builder = PipelineBuilder::create(
            build_side_context,
            self.enable_profiling,
            self.prof_span_set.clone(),
        );
        let mut build_res = build_side_builder.finalize(build)?;

        assert!(build_res.main_pipeline.is_pulling_pipeline()?);
        let create_sink_processor = |input| {
            let transform = TransformHashJoinBuild::create(
                input,
                TransformHashJoinBuild::attach(join_state.clone())?,
            );

            if self.enable_profiling {
                Ok(ProcessorPtr::create(ProfileWrapper::create(
                    transform,
                    hash_join_plan.plan_id,
                    self.prof_span_set.clone(),
                )))
            } else {
                Ok(ProcessorPtr::create(transform))
            }
        };
        if hash_join_plan.contain_runtime_filter {
            build_res.main_pipeline.duplicate(false)?;
            self.join_state = Some(join_state);
            self.index = Some(self.pipelines.len());
        } else {
            build_res.main_pipeline.add_sink(create_sink_processor)?;
        }

        self.pipelines.push(build_res.main_pipeline);
        self.pipelines
            .extend(build_res.sources_pipelines.into_iter());
        Ok(())
    }

    pub fn render_result_set(
        func_ctx: &FunctionContext,
        input_schema: DataSchemaRef,
        result_columns: &[ColumnBinding],
        pipeline: &mut Pipeline,
        ignore_result: bool,
    ) -> Result<()> {
        if ignore_result {
            return pipeline.add_sink(|input| Ok(ProcessorPtr::create(EmptySink::create(input))));
        }

        let mut projections = Vec::with_capacity(result_columns.len());

        for column_binding in result_columns {
            let index = column_binding.index;
            projections.push(input_schema.index_of(index.to_string().as_str())?);
        }
        let num_input_columns = input_schema.num_fields();
        pipeline.add_transform(|input, output| {
            Ok(ProcessorPtr::create(CompoundBlockOperator::create(
                input,
                output,
                num_input_columns,
                func_ctx.clone(),
                vec![BlockOperator::Project {
                    projection: projections.clone(),
                }],
            )))
        })?;

        Ok(())
    }

    fn build_table_scan(&mut self, scan: &TableScan) -> Result<()> {
        let table = self.ctx.build_table_from_source_plan(&scan.source)?;
        self.ctx.set_partitions(scan.source.parts.clone())?;
        table.read_data(self.ctx.clone(), &scan.source, &mut self.main_pipeline)?;

        // Fill internal columns if needed.
        if let Some(internal_columns) = &scan.internal_column {
            if table.support_row_id_column() {
                self.main_pipeline.add_transform(|input, output| {
                    Ok(ProcessorPtr::create(Box::new(
                        FillInternalColumnProcessor::create(
                            internal_columns.clone(),
                            input,
                            output,
                        ),
                    )))
                })?;
            } else {
                return Err(ErrorCode::TableEngineNotSupported(format!(
                    "Table engine `{}` does not support virtual column _row_id",
                    table.engine()
                )));
            }
        }

        let schema = scan.source.schema();
        let projection = scan
            .name_mapping
            .keys()
            .map(|name| schema.index_of(name.as_str()))
            .collect::<Result<Vec<usize>>>()?;

        // if projection is sequential, no need to add projection
        if projection != (0..schema.fields().len()).collect::<Vec<usize>>() {
            let ops = vec![BlockOperator::Project { projection }];
            let func_ctx = self.ctx.get_function_context()?;

            let num_input_columns = schema.num_fields();
            self.main_pipeline.add_transform(|input, output| {
                Ok(ProcessorPtr::create(CompoundBlockOperator::create(
                    input,
                    output,
                    num_input_columns,
                    func_ctx.clone(),
                    ops.clone(),
                )))
            })?;
        }

        Ok(())
    }

    fn build_filter(&mut self, filter: &Filter) -> Result<()> {
        self.build_pipeline(&filter.input)?;

        let predicate = filter
            .predicates
            .iter()
            .map(|expr| expr.as_expr(&BUILTIN_FUNCTIONS))
            .try_reduce(|lhs, rhs| {
                check_function(None, "and_filters", &[], &[lhs, rhs], &BUILTIN_FUNCTIONS)
            })
            .transpose()
            .unwrap_or_else(|| {
                Err(ErrorCode::Internal(
                    "Invalid empty predicate list".to_string(),
                ))
            })?;

        let num_input_columns = filter.input.output_schema()?.num_fields();
        self.main_pipeline.add_transform(|input, output| {
            let transform = CompoundBlockOperator::create(
                input,
                output,
                num_input_columns,
                self.ctx.get_function_context()?,
                vec![BlockOperator::Filter {
                    expr: predicate.clone(),
                }],
            );

            if self.enable_profiling {
                Ok(ProcessorPtr::create(ProfileWrapper::create(
                    transform,
                    filter.plan_id,
                    self.prof_span_set.clone(),
                )))
            } else {
                Ok(ProcessorPtr::create(transform))
            }
        })?;

        Ok(())
    }

    fn build_project(&mut self, project: &Project) -> Result<()> {
        self.build_pipeline(&project.input)?;
        let func_ctx = self.ctx.get_function_context()?;

        let num_input_columns = project.input.output_schema()?.num_fields();

        self.main_pipeline.add_transform(|input, output| {
            Ok(ProcessorPtr::create(CompoundBlockOperator::create(
                input,
                output,
                num_input_columns,
                func_ctx.clone(),
                vec![BlockOperator::Project {
                    projection: project.projections.clone(),
                }],
            )))
        })
    }

    fn build_eval_scalar(&mut self, eval_scalar: &EvalScalar) -> Result<()> {
        self.build_pipeline(&eval_scalar.input)?;

        let input_schema = eval_scalar.input.output_schema()?;
        let exprs = eval_scalar
            .exprs
            .iter()
            .filter(|(scalar, idx)| {
                if let RemoteExpr::ColumnRef { id, .. } = scalar {
                    return idx.to_string() != input_schema.field(*id).name().as_str();
                }
                true
            })
            .map(|(scalar, _)| scalar.as_expr(&BUILTIN_FUNCTIONS))
            .collect::<Vec<_>>();

        if exprs.is_empty() {
            return Ok(());
        }

        let op = BlockOperator::Map { exprs };

        let func_ctx = self.ctx.get_function_context()?;

        let num_input_columns = input_schema.num_fields();

        self.main_pipeline.add_transform(|input, output| {
            let transform = CompoundBlockOperator::create(
                input,
                output,
                num_input_columns,
                func_ctx.clone(),
                vec![op.clone()],
            );

            if self.enable_profiling {
                Ok(ProcessorPtr::create(ProfileWrapper::create(
                    transform,
                    eval_scalar.plan_id,
                    self.prof_span_set.clone(),
                )))
            } else {
                Ok(ProcessorPtr::create(transform))
            }
        })?;

        Ok(())
    }

    fn build_project_set(&mut self, project_set: &ProjectSet) -> Result<()> {
        self.build_pipeline(&project_set.input)?;

        let op = BlockOperator::FlatMap {
            srf_exprs: project_set
                .srf_exprs
                .iter()
                .map(|(expr, _)| expr.as_expr(&BUILTIN_FUNCTIONS))
                .collect(),
        };

        let func_ctx = self.ctx.get_function_context()?;

        let num_input_columns = project_set.input.output_schema()?.num_fields();

        self.main_pipeline.add_transform(|input, output| {
            let transform = CompoundBlockOperator::create(
                input,
                output,
                num_input_columns,
                func_ctx.clone(),
                vec![op.clone()],
            );

            if self.enable_profiling {
                Ok(ProcessorPtr::create(ProfileWrapper::create(
                    transform,
                    project_set.plan_id,
                    self.prof_span_set.clone(),
                )))
            } else {
                Ok(ProcessorPtr::create(transform))
            }
        })
    }

    fn build_aggregate_expand(&mut self, expand: &AggregateExpand) -> Result<()> {
        self.build_pipeline(&expand.input)?;
        let input_schema = expand.input.output_schema()?;
        let group_bys = expand
            .group_bys
            .iter()
            .take(expand.group_bys.len() - 1) // The last group-by will be virtual column `_grouping_id`
            .map(|i| {
                let index = input_schema.index_of(&i.to_string())?;
                let ty = input_schema.field(index).data_type();
                Ok((index, ty.clone()))
            })
            .collect::<Result<Vec<_>>>()?;
        let grouping_sets = expand
            .grouping_sets
            .iter()
            .map(|sets| {
                sets.iter()
                    .map(|i| {
                        let i = input_schema.index_of(&i.to_string())?;
                        let offset = group_bys.iter().position(|(j, _)| *j == i).unwrap();
                        Ok(offset)
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        let mut grouping_ids = Vec::with_capacity(grouping_sets.len());
        let mask = (1 << group_bys.len()) - 1;
        for set in grouping_sets {
            let mut id = 0;
            for i in set {
                id |= 1 << i;
            }
            // For element in `group_bys`,
            // if it is in current grouping set: set 0, else: set 1. (1 represents it will be NULL in grouping)
            // Example: GROUP BY GROUPING SETS ((a, b), (a), (b), ())
            // group_bys: [a, b]
            // grouping_sets: [[0, 1], [0], [1], []]
            // grouping_ids: 00, 01, 10, 11
            grouping_ids.push(!id & mask);
        }

        self.main_pipeline.add_transform(|input, output| {
            Ok(TransformExpandGroupingSets::create(
                input,
                output,
                group_bys.clone(),
                grouping_ids.clone(),
            ))
        })
    }

    fn build_aggregate_partial(&mut self, aggregate: &AggregatePartial) -> Result<()> {
        self.build_pipeline(&aggregate.input)?;

        let params = Self::build_aggregator_params(
            aggregate.input.output_schema()?,
            &aggregate.group_by,
            &aggregate.agg_funcs,
            None,
        )?;

        if params.group_columns.is_empty() {
            return self.main_pipeline.add_transform(|input, output| {
                let transform = PartialSingleStateAggregator::try_create(input, output, &params)?;

                if self.enable_profiling {
                    Ok(ProcessorPtr::create(ProfileWrapper::create(
                        transform,
                        aggregate.plan_id,
                        self.prof_span_set.clone(),
                    )))
                } else {
                    Ok(ProcessorPtr::create(transform))
                }
            });
        }

        // let is_standalone = self.ctx.get_cluster().is_empty();
        let settings = self.ctx.get_settings();
        let efficiently_memory = settings.get_efficiently_memory_group_by()?;

        let group_cols = &params.group_columns;
        let schema_before_group_by = params.input_schema.clone();
        let sample_block = DataBlock::empty_with_schema(schema_before_group_by);
        let method = DataBlock::choose_hash_method(&sample_block, group_cols, efficiently_memory)?;

        self.main_pipeline.add_transform(|input, output| {
            let transform = match params.aggregate_functions.is_empty() {
                true => with_mappedhash_method!(|T| match method.clone() {
                    HashMethodKind::T(method) => TransformPartialGroupBy::try_create(
                        self.ctx.clone(),
                        method,
                        input,
                        output,
                        params.clone()
                    ),
                }),
                false => with_mappedhash_method!(|T| match method.clone() {
                    HashMethodKind::T(method) => TransformPartialAggregate::try_create(
                        self.ctx.clone(),
                        method,
                        input,
                        output,
                        params.clone()
                    ),
                }),
            }?;

            if self.enable_profiling {
                Ok(ProcessorPtr::create(ProfileWrapper::create(
                    transform,
                    aggregate.plan_id,
                    self.prof_span_set.clone(),
                )))
            } else {
                Ok(ProcessorPtr::create(transform))
            }
        })?;

        // If cluster mode, spill write will be completed in exchange serialize, because we need scatter the block data first
        if self.ctx.get_cluster().is_empty()
            && !self
                .ctx
                .get_settings()
                .get_spilling_bytes_threshold_per_proc()?
                .is_zero()
        {
            let operator = DataOperator::instance().operator();
            let location_prefix = format!("_aggregate_spill/{}", self.ctx.get_tenant());
            self.main_pipeline.add_transform(|input, output| {
                let transform = match params.aggregate_functions.is_empty() {
                    true => with_mappedhash_method!(|T| match method.clone() {
                        HashMethodKind::T(method) => TransformGroupBySpillWriter::create(
                            input,
                            output,
                            method,
                            operator.clone(),
                            location_prefix.clone()
                        ),
                    }),
                    false => with_mappedhash_method!(|T| match method.clone() {
                        HashMethodKind::T(method) => TransformAggregateSpillWriter::create(
                            input,
                            output,
                            method,
                            operator.clone(),
                            params.clone(),
                            location_prefix.clone()
                        ),
                    }),
                };

                if self.enable_profiling {
                    Ok(ProcessorPtr::create(ProfileWrapper::create(
                        transform,
                        aggregate.plan_id,
                        self.prof_span_set.clone(),
                    )))
                } else {
                    Ok(ProcessorPtr::create(transform))
                }
            })?;
        }

        let tenant = self.ctx.get_tenant();
        self.exchange_injector = match params.aggregate_functions.is_empty() {
            true => with_mappedhash_method!(|T| match method.clone() {
                HashMethodKind::T(method) => AggregateInjector::<_, ()>::create(
                    &self.ctx,
                    tenant.clone(),
                    method,
                    params.clone()
                ),
            }),
            false => with_mappedhash_method!(|T| match method.clone() {
                HashMethodKind::T(method) => AggregateInjector::<_, usize>::create(
                    &self.ctx,
                    tenant.clone(),
                    method,
                    params.clone()
                ),
            }),
        };

        Ok(())
    }

    fn build_aggregate_final(&mut self, aggregate: &AggregateFinal) -> Result<()> {
        let params = Self::build_aggregator_params(
            aggregate.before_group_by_schema.clone(),
            &aggregate.group_by,
            &aggregate.agg_funcs,
            aggregate.limit,
        )?;

        if params.group_columns.is_empty() {
            self.build_pipeline(&aggregate.input)?;
            self.main_pipeline.try_resize(1)?;
            return self.main_pipeline.add_transform(|input, output| {
                let transform = FinalSingleStateAggregator::try_create(input, output, &params)?;

                if self.enable_profiling {
                    Ok(ProcessorPtr::create(ProfileWrapper::create(
                        transform,
                        aggregate.plan_id,
                        self.prof_span_set.clone(),
                    )))
                } else {
                    Ok(ProcessorPtr::create(transform))
                }
            });
        }

        let settings = self.ctx.get_settings();
        let efficiently_memory = settings.get_efficiently_memory_group_by()?;

        let group_cols = &params.group_columns;
        let schema_before_group_by = params.input_schema.clone();
        let sample_block = DataBlock::empty_with_schema(schema_before_group_by);
        let method = DataBlock::choose_hash_method(&sample_block, group_cols, efficiently_memory)?;

        let tenant = self.ctx.get_tenant();
        let old_inject = self.exchange_injector.clone();

        match params.aggregate_functions.is_empty() {
            true => with_hash_method!(|T| match method {
                HashMethodKind::T(v) => {
                    let input: &PhysicalPlan = &aggregate.input;
                    if matches!(input, PhysicalPlan::ExchangeSource(_)) {
                        self.exchange_injector = AggregateInjector::<_, ()>::create(
                            &self.ctx,
                            tenant,
                            v.clone(),
                            params.clone(),
                        );
                    }

                    self.build_pipeline(&aggregate.input)?;
                    self.exchange_injector = old_inject;
                    build_partition_bucket::<_, ()>(
                        &self.ctx,
                        v,
                        &mut self.main_pipeline,
                        params.clone(),
                    )
                }
            }),
            false => with_hash_method!(|T| match method {
                HashMethodKind::T(v) => {
                    let input: &PhysicalPlan = &aggregate.input;
                    if matches!(input, PhysicalPlan::ExchangeSource(_)) {
                        self.exchange_injector = AggregateInjector::<_, usize>::create(
                            &self.ctx,
                            tenant,
                            v.clone(),
                            params.clone(),
                        );
                    }
                    self.build_pipeline(&aggregate.input)?;
                    self.exchange_injector = old_inject;
                    build_partition_bucket::<_, usize>(
                        &self.ctx,
                        v,
                        &mut self.main_pipeline,
                        params.clone(),
                    )
                }
            }),
        }
    }

    pub fn build_aggregator_params(
        input_schema: DataSchemaRef,
        group_by: &[IndexType],
        agg_funcs: &[AggregateFunctionDesc],
        limit: Option<usize>,
    ) -> Result<Arc<AggregatorParams>> {
        let mut agg_args = Vec::with_capacity(agg_funcs.len());
        let (group_by, group_data_types) = group_by
            .iter()
            .map(|i| {
                let index = input_schema.index_of(&i.to_string())?;
                Ok((index, input_schema.field(index).data_type().clone()))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .unzip::<_, _, Vec<_>, Vec<_>>();

        let aggs: Vec<AggregateFunctionRef> = agg_funcs
            .iter()
            .map(|agg_func| {
                agg_args.push(agg_func.args.clone());
                AggregateFunctionFactory::instance().get(
                    agg_func.sig.name.as_str(),
                    agg_func.sig.params.clone(),
                    agg_func.sig.args.clone(),
                )
            })
            .collect::<Result<_>>()?;

        let params = AggregatorParams::try_create(
            input_schema,
            group_data_types,
            &group_by,
            &aggs,
            &agg_args,
            limit,
        )?;

        Ok(params)
    }

    fn build_window(&mut self, window: &Window) -> Result<()> {
        self.build_pipeline(&window.input)?;

        let input_schema = window.input.output_schema()?;

        let partition_by = window
            .partition_by
            .iter()
            .map(|p| {
                let offset = input_schema.index_of(&p.to_string())?;
                Ok(offset)
            })
            .collect::<Result<Vec<_>>>()?;

        let order_by = window
            .order_by
            .iter()
            .map(|o| {
                let offset = input_schema.index_of(&o.order_by.to_string())?;
                Ok(SortColumnDescription {
                    offset,
                    asc: o.asc,
                    nulls_first: o.nulls_first,
                    is_nullable: input_schema.field(offset).is_nullable(), // Used for check null frame.
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let old_output_len = self.main_pipeline.output_len();
        if !partition_by.is_empty() || !order_by.is_empty() {
            let mut sort_desc = Vec::with_capacity(partition_by.len() + order_by.len());

            for offset in &partition_by {
                sort_desc.push(SortColumnDescription {
                    offset: *offset,
                    asc: true,
                    nulls_first: true,
                    is_nullable: input_schema.field(*offset).is_nullable(),  // This information is not needed here.
                })
            }

            sort_desc.extend(order_by.clone());

            self.build_sort_pipeline(input_schema.clone(), sort_desc, window.plan_id, None, false)?;
        }
        // `TransformWindow` is a pipeline breaker.
        self.main_pipeline.try_resize(1)?;
        let func = WindowFunctionInfo::try_create(&window.func, &input_schema)?;
        // Window
        self.main_pipeline.add_transform(|input, output| {
            // The transform can only be created here, because it cannot be cloned.

            let transform = if window.window_frame.units.is_rows() {
                let start_bound = FrameBound::try_from(&window.window_frame.start_bound)?;
                let end_bound = FrameBound::try_from(&window.window_frame.end_bound)?;
                Box::new(TransformWindow::<u64>::try_create_rows(
                    input,
                    output,
                    func.clone(),
                    partition_by.clone(),
                    order_by.clone(),
                    (start_bound, end_bound),
                )?) as Box<dyn Processor>
            } else {
                if order_by.len() == 1 {
                    // If the length of order_by is 1, there may be a RANGE frame.
                    let data_type = input_schema
                        .field(order_by[0].offset)
                        .data_type()
                        .remove_nullable();
                    with_number_mapped_type!(|NUM_TYPE| match data_type {
                        DataType::Number(NumberDataType::NUM_TYPE) => {
                            let start_bound =
                                FrameBound::try_from(&window.window_frame.start_bound)?;
                            let end_bound = FrameBound::try_from(&window.window_frame.end_bound)?;
                            return Ok(ProcessorPtr::create(Box::new(
                                TransformWindow::<NUM_TYPE>::try_create_range(
                                    input,
                                    output,
                                    func.clone(),
                                    partition_by.clone(),
                                    order_by.clone(),
                                    (start_bound, end_bound),
                                )?,
                            )
                                as Box<dyn Processor>));
                        }
                        _ => {}
                    })
                }

                // There is no offset in the RANGE frame. (just CURRENT ROW or UNBOUNDED)
                // So we can use any number type to create the transform.
                let start_bound = FrameBound::try_from(&window.window_frame.start_bound)?;
                let end_bound = FrameBound::try_from(&window.window_frame.end_bound)?;
                Box::new(TransformWindow::<u8>::try_create_range(
                    input,
                    output,
                    func.clone(),
                    partition_by.clone(),
                    order_by.clone(),
                    (start_bound, end_bound),
                )?) as Box<dyn Processor>
            };
            Ok(ProcessorPtr::create(transform))
        })?;

        self.main_pipeline.try_resize(old_output_len)
    }

    fn build_sort(&mut self, sort: &Sort) -> Result<()> {
        self.build_pipeline(&sort.input)?;

        let input_schema = sort.input.output_schema()?;

        let sort_desc = sort
            .order_by
            .iter()
            .map(|desc| {
                let offset = input_schema.index_of(&desc.order_by.to_string())?;
                Ok(SortColumnDescription {
                    offset,
                    asc: desc.asc,
                    nulls_first: desc.nulls_first,
                    is_nullable: input_schema.field(offset).is_nullable(),  // This information is not needed here.
                })
            })
            .collect::<Result<Vec<_>>>()?;

        self.build_sort_pipeline(
            input_schema,
            sort_desc,
            sort.plan_id,
            sort.limit,
            sort.after_exchange,
        )
    }

    fn build_sort_pipeline(
        &mut self,
        input_schema: DataSchemaRef,
        sort_desc: Vec<SortColumnDescription>,
        plan_id: u32,
        limit: Option<usize>,
        after_exchange: bool,
    ) -> Result<()> {
        let block_size = self.ctx.get_settings().get_max_block_size()? as usize;
        let max_threads = self.ctx.get_settings().get_max_threads()? as usize;

        // TODO(Winter): the query will hang in MultiSortMergeProcessor when max_threads == 1 and output_len != 1
        if self.main_pipeline.output_len() == 1 || max_threads == 1 {
            self.main_pipeline.try_resize(max_threads)?;
        }
        let prof_info = if self.enable_profiling {
            Some((plan_id, self.prof_span_set.clone()))
        } else {
            None
        };

        build_full_sort_pipeline(
            &mut self.main_pipeline,
            input_schema,
            sort_desc,
            limit,
            block_size,
            block_size,
            prof_info,
            after_exchange,
        )
    }

    fn build_limit(&mut self, limit: &Limit) -> Result<()> {
        self.build_pipeline(&limit.input)?;

        self.main_pipeline.try_resize(1)?;
        self.main_pipeline.add_transform(|input, output| {
            let transform = TransformLimit::try_create(limit.limit, limit.offset, input, output)?;

            if self.enable_profiling {
                Ok(ProcessorPtr::create(ProfileWrapper::create(
                    transform,
                    limit.plan_id,
                    self.prof_span_set.clone(),
                )))
            } else {
                Ok(ProcessorPtr::create(transform))
            }
        })
    }

    fn build_row_fetch(&mut self, row_fetch: &RowFetch) -> Result<()> {
        debug_assert!(matches!(&*row_fetch.input, PhysicalPlan::Limit(_)));
        self.build_pipeline(&row_fetch.input)?;
        build_row_fetcher_pipeline(
            self.ctx.clone(),
            &mut self.main_pipeline,
            row_fetch.row_id_col_offset,
            &row_fetch.source,
            row_fetch.cols_to_fetch.clone(),
        )
    }

    fn build_join_probe(&mut self, join: &HashJoin, state: Arc<JoinHashTable>) -> Result<()> {
        self.build_pipeline(&join.probe)?;

        self.main_pipeline.add_transform(|input, output| {
            let transform = TransformHashJoinProbe::create(
                self.ctx.clone(),
                input,
                output,
                TransformHashJoinProbe::attach(state.clone())?,
                &join.join_type,
                !join.non_equi_conditions.is_empty(),
            )?;

            if self.enable_profiling {
                Ok(ProcessorPtr::create(ProfileWrapper::create(
                    transform,
                    join.plan_id,
                    self.prof_span_set.clone(),
                )))
            } else {
                Ok(ProcessorPtr::create(transform))
            }
        })?;

        if join.join_type == JoinType::LeftMark {
            self.main_pipeline.try_resize(1)?;
            self.main_pipeline.add_transform(|input, output| {
                let transform = TransformMarkJoin::try_create(
                    input,
                    output,
                    MarkJoinCompactor::create(state.clone()),
                )?;

                if self.enable_profiling {
                    Ok(ProcessorPtr::create(ProfileWrapper::create(
                        transform,
                        join.plan_id,
                        self.prof_span_set.clone(),
                    )))
                } else {
                    Ok(ProcessorPtr::create(transform))
                }
            })?;
        }

        Ok(())
    }

    pub fn build_exchange_source(&mut self, exchange_source: &ExchangeSource) -> Result<()> {
        let exchange_manager = self.ctx.get_exchange_manager();
        let build_res = exchange_manager.get_fragment_source(
            &exchange_source.query_id,
            exchange_source.source_fragment_id,
            self.enable_profiling,
            self.exchange_injector.clone(),
        )?;
        self.main_pipeline = build_res.main_pipeline;
        self.pipelines.extend(build_res.sources_pipelines);
        Ok(())
    }

    pub fn build_exchange_sink(&mut self, exchange_sink: &ExchangeSink) -> Result<()> {
        // ExchangeSink will be appended by `ExchangeManager::execute_pipeline`
        self.build_pipeline(&exchange_sink.input)
    }

    fn expand_union_all(
        &mut self,
        input: &PhysicalPlan,
        union_plan: &UnionAll,
    ) -> Result<Receiver<DataBlock>> {
        let union_ctx = QueryContext::create_from(self.ctx.clone());
        let pipeline_builder =
            PipelineBuilder::create(union_ctx, self.enable_profiling, self.prof_span_set.clone());
        let mut build_res = pipeline_builder.finalize(input)?;

        assert!(build_res.main_pipeline.is_pulling_pipeline()?);

        let (tx, rx) = async_channel::unbounded();

        build_res.main_pipeline.add_sink(|input_port| {
            let transform = UnionReceiveSink::create(Some(tx.clone()), input_port);

            if self.enable_profiling {
                Ok(ProcessorPtr::create(ProfileWrapper::create(
                    transform,
                    union_plan.plan_id,
                    self.prof_span_set.clone(),
                )))
            } else {
                Ok(ProcessorPtr::create(transform))
            }
        })?;

        self.pipelines.push(build_res.main_pipeline);
        self.pipelines
            .extend(build_res.sources_pipelines.into_iter());
        Ok(rx)
    }

    pub fn build_union_all(&mut self, union_all: &UnionAll) -> Result<()> {
        self.build_pipeline(&union_all.left)?;
        let union_all_receiver = self.expand_union_all(&union_all.right, union_all)?;
        self.main_pipeline
            .add_transform(|transform_input_port, transform_output_port| {
                let transform = TransformMergeBlock::try_create(
                    transform_input_port,
                    transform_output_port,
                    union_all.left.output_schema()?,
                    union_all.right.output_schema()?,
                    union_all.pairs.clone(),
                    union_all_receiver.clone(),
                )?;

                if self.enable_profiling {
                    Ok(ProcessorPtr::create(ProfileWrapper::create(
                        transform,
                        union_all.plan_id,
                        self.prof_span_set.clone(),
                    )))
                } else {
                    Ok(ProcessorPtr::create(transform))
                }
            })?;
        Ok(())
    }

    pub fn build_distributed_insert_select(
        &mut self,
        insert_select: &DistributedInsertSelect,
    ) -> Result<()> {
        let select_schema = &insert_select.select_schema;
        let insert_schema = &insert_select.insert_schema;

        self.build_pipeline(&insert_select.input)?;

        // should render result for select
        PipelineBuilder::render_result_set(
            &self.ctx.get_function_context()?,
            insert_select.input.output_schema()?,
            &insert_select.select_column_bindings,
            &mut self.main_pipeline,
            false,
        )?;

        if insert_select.cast_needed {
            let func_ctx = self.ctx.get_function_context()?;
            self.main_pipeline
                .add_transform(|transform_input_port, transform_output_port| {
                    TransformCastSchema::try_create(
                        transform_input_port,
                        transform_output_port,
                        select_schema.clone(),
                        insert_schema.clone(),
                        func_ctx.clone(),
                    )
                })?;
        }

        let table = self
            .ctx
            .get_catalog(&insert_select.catalog)?
            .get_table_by_info(&insert_select.table_info)?;

        let source_schema = insert_schema;
        fill_missing_columns(
            self.ctx.clone(),
            table.clone(),
            source_schema.clone(),
            &mut self.main_pipeline,
        )?;

        table.append_data(
            self.ctx.clone(),
            &mut self.main_pipeline,
            AppendMode::Normal,
        )?;

        Ok(())
    }

    pub fn build_runtime_filter_source(
        &mut self,
        runtime_filter_source: &RuntimeFilterSource,
    ) -> Result<()> {
        let state = self.build_runtime_filter_state(self.ctx.clone(), runtime_filter_source)?;
        self.expand_runtime_filter_source(&runtime_filter_source.right_side, state.clone())?;
        self.build_runtime_filter(&runtime_filter_source.left_side, state)?;
        Ok(())
    }

    fn expand_runtime_filter_source(
        &mut self,
        _right_side: &PhysicalPlan,
        state: Arc<RuntimeFilterState>,
    ) -> Result<()> {
        let pipeline = &mut self.pipelines[self.index.unwrap()];
        let output_size = pipeline.output_len();
        debug_assert!(output_size % 2 == 0);
        let mut items = Vec::with_capacity(output_size);
        //           Join
        //          /   \
        //        /      \
        //   RFSource     \
        //      /    \     \
        //     /      \     \
        // scan t1     scan t2
        for _ in 0..output_size / 2 {
            let input = InputPort::create();
            items.push(PipeItem::create(
                ProcessorPtr::create(TransformHashJoinBuild::create(
                    input.clone(),
                    TransformHashJoinBuild::attach(self.join_state.as_ref().unwrap().clone())?,
                )),
                vec![input],
                vec![],
            ));
            let input = InputPort::create();
            items.push(PipeItem::create(
                ProcessorPtr::create(Sinker::<SinkRuntimeFilterSource>::create(
                    input.clone(),
                    SinkRuntimeFilterSource::new(state.clone()),
                )),
                vec![input],
                vec![],
            ));
        }
        pipeline.add_pipe(Pipe::create(output_size, 0, items));
        Ok(())
    }

    fn build_runtime_filter(
        &mut self,
        left_side: &PhysicalPlan,
        state: Arc<RuntimeFilterState>,
    ) -> Result<()> {
        self.build_pipeline(left_side)?;
        self.main_pipeline.add_transform(|input, output| {
            let processor = TransformRuntimeFilter::create(input, output, state.clone());
            Ok(ProcessorPtr::create(processor))
        })?;
        Ok(())
    }

    fn build_runtime_filter_state(
        &self,
        ctx: Arc<QueryContext>,
        runtime_filter_source: &RuntimeFilterSource,
    ) -> Result<Arc<RuntimeFilterState>> {
        Ok(Arc::new(RuntimeFilterState::new(
            ctx,
            runtime_filter_source.left_runtime_filters.clone(),
            runtime_filter_source.right_runtime_filters.clone(),
        )))
    }
}
