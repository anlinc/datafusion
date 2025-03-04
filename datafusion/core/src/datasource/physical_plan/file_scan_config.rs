// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! [`FileScanConfig`] to configure scanning of possibly partitioned
//! file sources.

use super::{
    get_projected_output_ordering, statistics::MinMaxStatistics, FileGroupsDisplay,
    FileStream,
};
use crate::datasource::file_format::file_compression_type::FileCompressionType;
use crate::datasource::{listing::PartitionedFile, object_store::ObjectStoreUrl};
use crate::{error::Result, scalar::ScalarValue};
use std::any::Any;
use std::fmt::Formatter;
use std::{fmt, sync::Arc};

use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion_common::stats::Precision;
use datafusion_common::{ColumnStatistics, Constraints, Statistics};
use datafusion_physical_expr::{EquivalenceProperties, LexOrdering, Partitioning};

use crate::datasource::data_source::FileSource;
pub use datafusion_datasource::file_scan_config::*;
use datafusion_datasource::source::{DataSource, DataSourceExec};
use datafusion_execution::{SendableRecordBatchStream, TaskContext};
use datafusion_physical_plan::display::{display_orderings, ProjectSchemaDisplay};
use datafusion_physical_plan::metrics::ExecutionPlanMetricsSet;
use datafusion_physical_plan::projection::{
    all_alias_free_columns, new_projections_for_columns, ProjectionExec,
};
use datafusion_physical_plan::{DisplayAs, DisplayFormatType, ExecutionPlan};

/// Convert type to a type suitable for use as a [`ListingTable`]
/// partition column. Returns `Dictionary(UInt16, val_type)`, which is
/// a reasonable trade off between a reasonable number of partition
/// values and space efficiency.
///
/// This use this to specify types for partition columns. However
/// you MAY also choose not to dictionary-encode the data or to use a
/// different dictionary type.
///
/// Use [`wrap_partition_value_in_dict`] to wrap a [`ScalarValue`] in the same say.
///
/// [`ListingTable`]: crate::datasource::listing::ListingTable
pub fn wrap_partition_type_in_dict(val_type: DataType) -> DataType {
    DataType::Dictionary(Box::new(DataType::UInt16), Box::new(val_type))
}

/// Convert a [`ScalarValue`] of partition columns to a type, as
/// described in the documentation of [`wrap_partition_type_in_dict`],
/// which can wrap the types.
pub fn wrap_partition_value_in_dict(val: ScalarValue) -> ScalarValue {
    ScalarValue::Dictionary(Box::new(DataType::UInt16), Box::new(val))
}

/// The base configurations for a [`DataSourceExec`], the a physical plan for
/// any given file format.
///
/// Use [`Self::build`] to create a [`DataSourceExec`] from a ``FileScanConfig`.
///
/// # Example
/// ```
/// # use std::sync::Arc;
/// # use arrow::datatypes::{Field, Fields, DataType, Schema};
/// # use datafusion::datasource::listing::PartitionedFile;
/// # use datafusion::datasource::physical_plan::FileScanConfig;
/// # use datafusion_execution::object_store::ObjectStoreUrl;
/// # use datafusion::datasource::physical_plan::ArrowSource;
/// # use datafusion_physical_plan::ExecutionPlan;
/// # let file_schema = Arc::new(Schema::new(vec![
/// #  Field::new("c1", DataType::Int32, false),
/// #  Field::new("c2", DataType::Int32, false),
/// #  Field::new("c3", DataType::Int32, false),
/// #  Field::new("c4", DataType::Int32, false),
/// # ]));
/// // create FileScan config for reading arrow files from file://
/// let object_store_url = ObjectStoreUrl::local_filesystem();
/// let file_source = Arc::new(ArrowSource::default());
/// let config = FileScanConfig::new(object_store_url, file_schema, file_source)
///   .with_limit(Some(1000))            // read only the first 1000 records
///   .with_projection(Some(vec![2, 3])) // project columns 2 and 3
///    // Read /tmp/file1.parquet with known size of 1234 bytes in a single group
///   .with_file(PartitionedFile::new("file1.parquet", 1234))
///   // Read /tmp/file2.parquet 56 bytes and /tmp/file3.parquet 78 bytes
///   // in a  single row group
///   .with_file_group(vec![
///    PartitionedFile::new("file2.parquet", 56),
///    PartitionedFile::new("file3.parquet", 78),
///   ]);
/// // create an execution plan from the config
/// let plan: Arc<dyn ExecutionPlan> = config.build();
/// ```
#[derive(Clone)]
pub struct FileScanConfig {
    /// Object store URL, used to get an [`ObjectStore`] instance from
    /// [`RuntimeEnv::object_store`]
    ///
    /// This `ObjectStoreUrl` should be the prefix of the absolute url for files
    /// as `file://` or `s3://my_bucket`. It should not include the path to the
    /// file itself. The relevant URL prefix must be registered via
    /// [`RuntimeEnv::register_object_store`]
    ///
    /// [`ObjectStore`]: object_store::ObjectStore
    /// [`RuntimeEnv::register_object_store`]: datafusion_execution::runtime_env::RuntimeEnv::register_object_store
    /// [`RuntimeEnv::object_store`]: datafusion_execution::runtime_env::RuntimeEnv::object_store
    pub object_store_url: ObjectStoreUrl,
    /// Schema before `projection` is applied. It contains the all columns that may
    /// appear in the files. It does not include table partition columns
    /// that may be added.
    pub file_schema: SchemaRef,
    /// List of files to be processed, grouped into partitions
    ///
    /// Each file must have a schema of `file_schema` or a subset. If
    /// a particular file has a subset, the missing columns are
    /// padded with NULLs.
    ///
    /// DataFusion may attempt to read each partition of files
    /// concurrently, however files *within* a partition will be read
    /// sequentially, one after the next.
    pub file_groups: Vec<Vec<PartitionedFile>>,
    /// Table constraints
    pub constraints: Constraints,
    /// Estimated overall statistics of the files, taking `filters` into account.
    /// Defaults to [`Statistics::new_unknown`].
    pub statistics: Statistics,
    /// Columns on which to project the data. Indexes that are higher than the
    /// number of columns of `file_schema` refer to `table_partition_cols`.
    pub projection: Option<Vec<usize>>,
    /// The maximum number of records to read from this plan. If `None`,
    /// all records after filtering are returned.
    pub limit: Option<usize>,
    /// The partitioning columns
    pub table_partition_cols: Vec<Field>,
    /// All equivalent lexicographical orderings that describe the schema.
    pub output_ordering: Vec<LexOrdering>,
    /// File compression type
    pub file_compression_type: FileCompressionType,
    /// Are new lines in values supported for CSVOptions
    pub new_lines_in_values: bool,
    /// File source such as `ParquetSource`, `CsvSource`, `JsonSource`, etc.
    pub source: Arc<dyn FileSource>,
}

impl DataSource for FileScanConfig {
    fn open(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let object_store = context.runtime_env().object_store(&self.object_store_url)?;

        let source = self
            .source
            .with_batch_size(context.session_config().batch_size())
            .with_schema(Arc::clone(&self.file_schema))
            .with_projection(self);

        let opener = source.create_file_opener(object_store, self, partition);

        let stream = FileStream::new(self, partition, opener, source.metrics())?;
        Ok(Box::pin(stream))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn fmt_as(&self, t: DisplayFormatType, f: &mut Formatter) -> fmt::Result {
        let (schema, _, _, orderings) = self.project();

        write!(f, "file_groups=")?;
        FileGroupsDisplay(&self.file_groups).fmt_as(t, f)?;

        if !schema.fields().is_empty() {
            write!(f, ", projection={}", ProjectSchemaDisplay(&schema))?;
        }

        if let Some(limit) = self.limit {
            write!(f, ", limit={limit}")?;
        }

        display_orderings(f, &orderings)?;

        if !self.constraints.is_empty() {
            write!(f, ", {}", self.constraints)?;
        }

        self.fmt_file_source(t, f)
    }

    /// If supported by the underlying [`FileSource`], redistribute files across partitions according to their size.
    fn repartitioned(
        &self,
        target_partitions: usize,
        repartition_file_min_size: usize,
        output_ordering: Option<LexOrdering>,
    ) -> Result<Option<Arc<dyn DataSource>>> {
        let source = self.source.repartitioned(
            target_partitions,
            repartition_file_min_size,
            output_ordering,
            self,
        )?;

        Ok(source.map(|s| Arc::new(s) as _))
    }

    fn output_partitioning(&self) -> Partitioning {
        Partitioning::UnknownPartitioning(self.file_groups.len())
    }

    fn eq_properties(&self) -> EquivalenceProperties {
        let (schema, constraints, _, orderings) = self.project();
        EquivalenceProperties::new_with_orderings(schema, orderings.as_slice())
            .with_constraints(constraints)
    }

    fn statistics(&self) -> Result<Statistics> {
        self.source.statistics()
    }

    fn with_fetch(&self, limit: Option<usize>) -> Option<Arc<dyn DataSource>> {
        let source = self.clone();
        Some(Arc::new(source.with_limit(limit)))
    }

    fn fetch(&self) -> Option<usize> {
        self.limit
    }

    fn metrics(&self) -> ExecutionPlanMetricsSet {
        self.source.metrics().clone()
    }

    fn try_swapping_with_projection(
        &self,
        projection: &ProjectionExec,
    ) -> Result<Option<Arc<dyn ExecutionPlan>>> {
        // If there is any non-column or alias-carrier expression, Projection should not be removed.
        // This process can be moved into CsvExec, but it would be an overlap of their responsibility.
        Ok(all_alias_free_columns(projection.expr()).then(|| {
            let file_scan = self.clone();
            let source = Arc::clone(&file_scan.source);
            let new_projections = new_projections_for_columns(
                projection,
                &file_scan
                    .projection
                    .clone()
                    .unwrap_or((0..self.file_schema.fields().len()).collect()),
            );
            file_scan
                // Assign projected statistics to source
                .with_projection(Some(new_projections))
                .with_source(source)
                .build() as _
        }))
    }
}

impl FileScanConfig {
    /// Create a new [`FileScanConfig`] with default settings for scanning files.
    ///
    /// See example on [`FileScanConfig`]
    ///
    /// No file groups are added by default. See [`Self::with_file`], [`Self::with_file_group`] and
    /// [`Self::with_file_groups`].
    ///
    /// # Parameters:
    /// * `object_store_url`: See [`Self::object_store_url`]
    /// * `file_schema`: See [`Self::file_schema`]
    pub fn new(
        object_store_url: ObjectStoreUrl,
        file_schema: SchemaRef,
        file_source: Arc<dyn FileSource>,
    ) -> Self {
        let statistics = Statistics::new_unknown(&file_schema);

        let mut config = Self {
            object_store_url,
            file_schema,
            file_groups: vec![],
            constraints: Constraints::empty(),
            statistics,
            projection: None,
            limit: None,
            table_partition_cols: vec![],
            output_ordering: vec![],
            file_compression_type: FileCompressionType::UNCOMPRESSED,
            new_lines_in_values: false,
            source: Arc::clone(&file_source),
        };

        config = config.with_source(Arc::clone(&file_source));
        config
    }

    /// Set the file source
    pub fn with_source(mut self, source: Arc<dyn FileSource>) -> Self {
        let (
            _projected_schema,
            _constraints,
            projected_statistics,
            _projected_output_ordering,
        ) = self.project();
        self.source = source.with_statistics(projected_statistics);
        self
    }

    /// Set the table constraints of the files
    pub fn with_constraints(mut self, constraints: Constraints) -> Self {
        self.constraints = constraints;
        self
    }

    /// Set the statistics of the files
    pub fn with_statistics(mut self, statistics: Statistics) -> Self {
        self.statistics = statistics;
        self
    }

    /// Set the projection of the files
    pub fn with_projection(mut self, projection: Option<Vec<usize>>) -> Self {
        self.projection = projection;
        self
    }

    /// Set the limit of the files
    pub fn with_limit(mut self, limit: Option<usize>) -> Self {
        self.limit = limit;
        self
    }

    /// Add a file as a single group
    ///
    /// See [Self::file_groups] for more information.
    pub fn with_file(self, file: PartitionedFile) -> Self {
        self.with_file_group(vec![file])
    }

    /// Add the file groups
    ///
    /// See [Self::file_groups] for more information.
    pub fn with_file_groups(
        mut self,
        mut file_groups: Vec<Vec<PartitionedFile>>,
    ) -> Self {
        self.file_groups.append(&mut file_groups);
        self
    }

    /// Add a new file group
    ///
    /// See [Self::file_groups] for more information
    pub fn with_file_group(mut self, file_group: Vec<PartitionedFile>) -> Self {
        self.file_groups.push(file_group);
        self
    }

    /// Set the partitioning columns of the files
    pub fn with_table_partition_cols(mut self, table_partition_cols: Vec<Field>) -> Self {
        self.table_partition_cols = table_partition_cols;
        self
    }

    /// Set the output ordering of the files
    pub fn with_output_ordering(mut self, output_ordering: Vec<LexOrdering>) -> Self {
        self.output_ordering = output_ordering;
        self
    }

    /// Set the file compression type
    pub fn with_file_compression_type(
        mut self,
        file_compression_type: FileCompressionType,
    ) -> Self {
        self.file_compression_type = file_compression_type;
        self
    }

    /// Set the new_lines_in_values property
    pub fn with_newlines_in_values(mut self, new_lines_in_values: bool) -> Self {
        self.new_lines_in_values = new_lines_in_values;
        self
    }

    /// Specifies whether newlines in (quoted) values are supported.
    ///
    /// Parsing newlines in quoted values may be affected by execution behaviour such as
    /// parallel file scanning. Setting this to `true` ensures that newlines in values are
    /// parsed successfully, which may reduce performance.
    ///
    /// The default behaviour depends on the `datafusion.catalog.newlines_in_values` setting.
    pub fn newlines_in_values(&self) -> bool {
        self.new_lines_in_values
    }

    /// Project the schema, constraints, and the statistics on the given column indices
    pub fn project(&self) -> (SchemaRef, Constraints, Statistics, Vec<LexOrdering>) {
        if self.projection.is_none() && self.table_partition_cols.is_empty() {
            return (
                Arc::clone(&self.file_schema),
                self.constraints.clone(),
                self.statistics.clone(),
                self.output_ordering.clone(),
            );
        }

        let proj_indices = if let Some(proj) = &self.projection {
            proj
        } else {
            let len = self.file_schema.fields().len() + self.table_partition_cols.len();
            &(0..len).collect::<Vec<_>>()
        };

        let mut table_fields = vec![];
        let mut table_cols_stats = vec![];
        for idx in proj_indices {
            if *idx < self.file_schema.fields().len() {
                let field = self.file_schema.field(*idx);
                table_fields.push(field.clone());
                table_cols_stats.push(self.statistics.column_statistics[*idx].clone())
            } else {
                let partition_idx = idx - self.file_schema.fields().len();
                table_fields.push(self.table_partition_cols[partition_idx].to_owned());
                // TODO provide accurate stat for partition column (#1186)
                table_cols_stats.push(ColumnStatistics::new_unknown())
            }
        }

        let table_stats = Statistics {
            num_rows: self.statistics.num_rows,
            // TODO correct byte size?
            total_byte_size: Precision::Absent,
            column_statistics: table_cols_stats,
        };

        let projected_schema = Arc::new(Schema::new_with_metadata(
            table_fields,
            self.file_schema.metadata().clone(),
        ));

        let projected_constraints = self
            .constraints
            .project(proj_indices)
            .unwrap_or_else(Constraints::empty);

        let projected_output_ordering =
            get_projected_output_ordering(self, &projected_schema);

        (
            projected_schema,
            projected_constraints,
            table_stats,
            projected_output_ordering,
        )
    }

    #[cfg_attr(not(feature = "avro"), allow(unused))] // Only used by avro
    pub(crate) fn projected_file_column_names(&self) -> Option<Vec<String>> {
        self.projection.as_ref().map(|p| {
            p.iter()
                .filter(|col_idx| **col_idx < self.file_schema.fields().len())
                .map(|col_idx| self.file_schema.field(*col_idx).name())
                .cloned()
                .collect()
        })
    }

    /// Projects only file schema, ignoring partition columns
    pub(crate) fn projected_file_schema(&self) -> SchemaRef {
        let fields = self.file_column_projection_indices().map(|indices| {
            indices
                .iter()
                .map(|col_idx| self.file_schema.field(*col_idx))
                .cloned()
                .collect::<Vec<_>>()
        });

        fields.map_or_else(
            || Arc::clone(&self.file_schema),
            |f| {
                Arc::new(Schema::new_with_metadata(
                    f,
                    self.file_schema.metadata.clone(),
                ))
            },
        )
    }

    pub(crate) fn file_column_projection_indices(&self) -> Option<Vec<usize>> {
        self.projection.as_ref().map(|p| {
            p.iter()
                .filter(|col_idx| **col_idx < self.file_schema.fields().len())
                .copied()
                .collect()
        })
    }

    /// Attempts to do a bin-packing on files into file groups, such that any two files
    /// in a file group are ordered and non-overlapping with respect to their statistics.
    /// It will produce the smallest number of file groups possible.
    pub fn split_groups_by_statistics(
        table_schema: &SchemaRef,
        file_groups: &[Vec<PartitionedFile>],
        sort_order: &LexOrdering,
    ) -> Result<Vec<Vec<PartitionedFile>>> {
        let flattened_files = file_groups.iter().flatten().collect::<Vec<_>>();
        // First Fit:
        // * Choose the first file group that a file can be placed into.
        // * If it fits into no existing file groups, create a new one.
        //
        // By sorting files by min values and then applying first-fit bin packing,
        // we can produce the smallest number of file groups such that
        // files within a group are in order and non-overlapping.
        //
        // Source: Applied Combinatorics (Keller and Trotter), Chapter 6.8
        // https://www.appliedcombinatorics.org/book/s_posets_dilworth-intord.html

        if flattened_files.is_empty() {
            return Ok(vec![]);
        }

        let statistics = MinMaxStatistics::new_from_files(
            sort_order,
            table_schema,
            None,
            flattened_files.iter().copied(),
        )
        .map_err(|e| {
            e.context("construct min/max statistics for split_groups_by_statistics")
        })?;

        let indices_sorted_by_min = statistics.min_values_sorted();
        let mut file_groups_indices: Vec<Vec<usize>> = vec![];

        for (idx, min) in indices_sorted_by_min {
            let file_group_to_insert = file_groups_indices.iter_mut().find(|group| {
                // If our file is non-overlapping and comes _after_ the last file,
                // it fits in this file group.
                min > statistics.max(
                    *group
                        .last()
                        .expect("groups should be nonempty at construction"),
                )
            });
            match file_group_to_insert {
                Some(group) => group.push(idx),
                None => file_groups_indices.push(vec![idx]),
            }
        }

        // Assemble indices back into groups of PartitionedFiles
        Ok(file_groups_indices
            .into_iter()
            .map(|file_group_indices| {
                file_group_indices
                    .into_iter()
                    .map(|idx| flattened_files[idx].clone())
                    .collect()
            })
            .collect())
    }

    // TODO: This function should be moved into DataSourceExec once FileScanConfig moved out of datafusion/core
    /// Returns a new [`DataSourceExec`] to scan the files specified by this config
    pub fn build(self) -> Arc<DataSourceExec> {
        Arc::new(DataSourceExec::new(Arc::new(self)))
    }

    /// Write the data_type based on file_source
    fn fmt_file_source(&self, t: DisplayFormatType, f: &mut Formatter) -> fmt::Result {
        write!(f, ", file_type={}", self.source.file_type())?;
        self.source.fmt_extra(t, f)
    }

    /// Returns the file_source
    pub fn file_source(&self) -> &Arc<dyn FileSource> {
        &self.source
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasource::physical_plan::ArrowSource;
    use crate::{test::columns, test_util::aggr_test_schema};
    use arrow::array::{Int32Array, RecordBatch};
    use std::collections::HashMap;

    #[test]
    fn physical_plan_config_no_projection() {
        let file_schema = aggr_test_schema();
        let conf = config_for_projection(
            Arc::clone(&file_schema),
            None,
            Statistics::new_unknown(&file_schema),
            to_partition_cols(vec![(
                "date".to_owned(),
                wrap_partition_type_in_dict(DataType::Utf8),
            )]),
        );

        let (proj_schema, _, proj_statistics, _) = conf.project();
        assert_eq!(proj_schema.fields().len(), file_schema.fields().len() + 1);
        assert_eq!(
            proj_schema.field(file_schema.fields().len()).name(),
            "date",
            "partition columns are the last columns"
        );
        assert_eq!(
            proj_statistics.column_statistics.len(),
            file_schema.fields().len() + 1
        );
        // TODO implement tests for partition column statistics once implemented

        let col_names = conf.projected_file_column_names();
        assert_eq!(col_names, None);

        let col_indices = conf.file_column_projection_indices();
        assert_eq!(col_indices, None);
    }

    #[test]
    fn physical_plan_config_no_projection_tab_cols_as_field() {
        let file_schema = aggr_test_schema();

        // make a table_partition_col as a field
        let table_partition_col =
            Field::new("date", wrap_partition_type_in_dict(DataType::Utf8), true)
                .with_metadata(HashMap::from_iter(vec![(
                    "key_whatever".to_owned(),
                    "value_whatever".to_owned(),
                )]));

        let conf = config_for_projection(
            Arc::clone(&file_schema),
            None,
            Statistics::new_unknown(&file_schema),
            vec![table_partition_col.clone()],
        );

        // verify the proj_schema includes the last column and exactly the same the field it is defined
        let (proj_schema, _, _, _) = conf.project();
        assert_eq!(proj_schema.fields().len(), file_schema.fields().len() + 1);
        assert_eq!(
            *proj_schema.field(file_schema.fields().len()),
            table_partition_col,
            "partition columns are the last columns and ust have all values defined in created field"
        );
    }

    #[test]
    fn physical_plan_config_with_projection() {
        let file_schema = aggr_test_schema();
        let conf = config_for_projection(
            Arc::clone(&file_schema),
            Some(vec![file_schema.fields().len(), 0]),
            Statistics {
                num_rows: Precision::Inexact(10),
                // assign the column index to distinct_count to help assert
                // the source statistic after the projection
                column_statistics: (0..file_schema.fields().len())
                    .map(|i| ColumnStatistics {
                        distinct_count: Precision::Inexact(i),
                        ..Default::default()
                    })
                    .collect(),
                total_byte_size: Precision::Absent,
            },
            to_partition_cols(vec![(
                "date".to_owned(),
                wrap_partition_type_in_dict(DataType::Utf8),
            )]),
        );

        let (proj_schema, _, proj_statistics, _) = conf.project();
        assert_eq!(
            columns(&proj_schema),
            vec!["date".to_owned(), "c1".to_owned()]
        );
        let proj_stat_cols = proj_statistics.column_statistics;
        assert_eq!(proj_stat_cols.len(), 2);
        // TODO implement tests for proj_stat_cols[0] once partition column
        // statistics are implemented
        assert_eq!(proj_stat_cols[1].distinct_count, Precision::Inexact(0));

        let col_names = conf.projected_file_column_names();
        assert_eq!(col_names, Some(vec!["c1".to_owned()]));

        let col_indices = conf.file_column_projection_indices();
        assert_eq!(col_indices, Some(vec![0]));
    }

    #[test]
    fn partition_column_projector() {
        let file_batch = build_table_i32(
            ("a", &vec![0, 1, 2]),
            ("b", &vec![-2, -1, 0]),
            ("c", &vec![10, 11, 12]),
        );
        let partition_cols = vec![
            (
                "year".to_owned(),
                wrap_partition_type_in_dict(DataType::Utf8),
            ),
            (
                "month".to_owned(),
                wrap_partition_type_in_dict(DataType::Utf8),
            ),
            (
                "day".to_owned(),
                wrap_partition_type_in_dict(DataType::Utf8),
            ),
        ];
        // create a projected schema
        let conf = config_for_projection(
            file_batch.schema(),
            // keep all cols from file and 2 from partitioning
            Some(vec![
                0,
                1,
                2,
                file_batch.schema().fields().len(),
                file_batch.schema().fields().len() + 2,
            ]),
            Statistics::new_unknown(&file_batch.schema()),
            to_partition_cols(partition_cols.clone()),
        );
        let (proj_schema, ..) = conf.project();
        // created a projector for that projected schema
        let mut proj = PartitionColumnProjector::new(
            proj_schema,
            &partition_cols
                .iter()
                .map(|x| x.0.clone())
                .collect::<Vec<_>>(),
        );

        // project first batch
        let projected_batch = proj
            .project(
                // file_batch is ok here because we kept all the file cols in the projection
                file_batch,
                &[
                    wrap_partition_value_in_dict(ScalarValue::from("2021")),
                    wrap_partition_value_in_dict(ScalarValue::from("10")),
                    wrap_partition_value_in_dict(ScalarValue::from("26")),
                ],
            )
            .expect("Projection of partition columns into record batch failed");
        let expected = [
            "+---+----+----+------+-----+",
            "| a | b  | c  | year | day |",
            "+---+----+----+------+-----+",
            "| 0 | -2 | 10 | 2021 | 26  |",
            "| 1 | -1 | 11 | 2021 | 26  |",
            "| 2 | 0  | 12 | 2021 | 26  |",
            "+---+----+----+------+-----+",
        ];
        crate::assert_batches_eq!(expected, &[projected_batch]);

        // project another batch that is larger than the previous one
        let file_batch = build_table_i32(
            ("a", &vec![5, 6, 7, 8, 9]),
            ("b", &vec![-10, -9, -8, -7, -6]),
            ("c", &vec![12, 13, 14, 15, 16]),
        );
        let projected_batch = proj
            .project(
                // file_batch is ok here because we kept all the file cols in the projection
                file_batch,
                &[
                    wrap_partition_value_in_dict(ScalarValue::from("2021")),
                    wrap_partition_value_in_dict(ScalarValue::from("10")),
                    wrap_partition_value_in_dict(ScalarValue::from("27")),
                ],
            )
            .expect("Projection of partition columns into record batch failed");
        let expected = [
            "+---+-----+----+------+-----+",
            "| a | b   | c  | year | day |",
            "+---+-----+----+------+-----+",
            "| 5 | -10 | 12 | 2021 | 27  |",
            "| 6 | -9  | 13 | 2021 | 27  |",
            "| 7 | -8  | 14 | 2021 | 27  |",
            "| 8 | -7  | 15 | 2021 | 27  |",
            "| 9 | -6  | 16 | 2021 | 27  |",
            "+---+-----+----+------+-----+",
        ];
        crate::assert_batches_eq!(expected, &[projected_batch]);

        // project another batch that is smaller than the previous one
        let file_batch = build_table_i32(
            ("a", &vec![0, 1, 3]),
            ("b", &vec![2, 3, 4]),
            ("c", &vec![4, 5, 6]),
        );
        let projected_batch = proj
            .project(
                // file_batch is ok here because we kept all the file cols in the projection
                file_batch,
                &[
                    wrap_partition_value_in_dict(ScalarValue::from("2021")),
                    wrap_partition_value_in_dict(ScalarValue::from("10")),
                    wrap_partition_value_in_dict(ScalarValue::from("28")),
                ],
            )
            .expect("Projection of partition columns into record batch failed");
        let expected = [
            "+---+---+---+------+-----+",
            "| a | b | c | year | day |",
            "+---+---+---+------+-----+",
            "| 0 | 2 | 4 | 2021 | 28  |",
            "| 1 | 3 | 5 | 2021 | 28  |",
            "| 3 | 4 | 6 | 2021 | 28  |",
            "+---+---+---+------+-----+",
        ];
        crate::assert_batches_eq!(expected, &[projected_batch]);

        // forgot to dictionary-wrap the scalar value
        let file_batch = build_table_i32(
            ("a", &vec![0, 1, 2]),
            ("b", &vec![-2, -1, 0]),
            ("c", &vec![10, 11, 12]),
        );
        let projected_batch = proj
            .project(
                // file_batch is ok here because we kept all the file cols in the projection
                file_batch,
                &[
                    ScalarValue::from("2021"),
                    ScalarValue::from("10"),
                    ScalarValue::from("26"),
                ],
            )
            .expect("Projection of partition columns into record batch failed");
        let expected = [
            "+---+----+----+------+-----+",
            "| a | b  | c  | year | day |",
            "+---+----+----+------+-----+",
            "| 0 | -2 | 10 | 2021 | 26  |",
            "| 1 | -1 | 11 | 2021 | 26  |",
            "| 2 | 0  | 12 | 2021 | 26  |",
            "+---+----+----+------+-----+",
        ];
        crate::assert_batches_eq!(expected, &[projected_batch]);
    }

    #[test]
    fn test_projected_file_schema_with_partition_col() {
        let schema = aggr_test_schema();
        let partition_cols = vec![
            (
                "part1".to_owned(),
                wrap_partition_type_in_dict(DataType::Utf8),
            ),
            (
                "part2".to_owned(),
                wrap_partition_type_in_dict(DataType::Utf8),
            ),
        ];

        // Projected file schema for config with projection including partition column
        let projection = config_for_projection(
            schema.clone(),
            Some(vec![0, 3, 5, schema.fields().len()]),
            Statistics::new_unknown(&schema),
            to_partition_cols(partition_cols),
        )
        .projected_file_schema();

        // Assert partition column filtered out in projected file schema
        let expected_columns = vec!["c1", "c4", "c6"];
        let actual_columns = projection
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect::<Vec<_>>();
        assert_eq!(expected_columns, actual_columns);
    }

    #[test]
    fn test_projected_file_schema_without_projection() {
        let schema = aggr_test_schema();
        let partition_cols = vec![
            (
                "part1".to_owned(),
                wrap_partition_type_in_dict(DataType::Utf8),
            ),
            (
                "part2".to_owned(),
                wrap_partition_type_in_dict(DataType::Utf8),
            ),
        ];

        // Projected file schema for config without projection
        let projection = config_for_projection(
            schema.clone(),
            None,
            Statistics::new_unknown(&schema),
            to_partition_cols(partition_cols),
        )
        .projected_file_schema();

        // Assert projected file schema is equal to file schema
        assert_eq!(projection.fields(), schema.fields());
    }

    #[test]
    fn test_split_groups_by_statistics() -> Result<()> {
        use chrono::TimeZone;
        use datafusion_common::DFSchema;
        use datafusion_expr::execution_props::ExecutionProps;
        use object_store::{path::Path, ObjectMeta};

        struct File {
            name: &'static str,
            date: &'static str,
            statistics: Vec<Option<(f64, f64)>>,
        }
        impl File {
            fn new(
                name: &'static str,
                date: &'static str,
                statistics: Vec<Option<(f64, f64)>>,
            ) -> Self {
                Self {
                    name,
                    date,
                    statistics,
                }
            }
        }

        struct TestCase {
            name: &'static str,
            file_schema: Schema,
            files: Vec<File>,
            sort: Vec<datafusion_expr::SortExpr>,
            expected_result: Result<Vec<Vec<&'static str>>, &'static str>,
        }

        use datafusion_expr::col;
        let cases = vec![
            TestCase {
                name: "test sort",
                file_schema: Schema::new(vec![Field::new(
                    "value".to_string(),
                    DataType::Float64,
                    false,
                )]),
                files: vec![
                    File::new("0", "2023-01-01", vec![Some((0.00, 0.49))]),
                    File::new("1", "2023-01-01", vec![Some((0.50, 1.00))]),
                    File::new("2", "2023-01-02", vec![Some((0.00, 1.00))]),
                ],
                sort: vec![col("value").sort(true, false)],
                expected_result: Ok(vec![vec!["0", "1"], vec!["2"]]),
            },
            // same input but file '2' is in the middle
            // test that we still order correctly
            TestCase {
                name: "test sort with files ordered differently",
                file_schema: Schema::new(vec![Field::new(
                    "value".to_string(),
                    DataType::Float64,
                    false,
                )]),
                files: vec![
                    File::new("0", "2023-01-01", vec![Some((0.00, 0.49))]),
                    File::new("2", "2023-01-02", vec![Some((0.00, 1.00))]),
                    File::new("1", "2023-01-01", vec![Some((0.50, 1.00))]),
                ],
                sort: vec![col("value").sort(true, false)],
                expected_result: Ok(vec![vec!["0", "1"], vec!["2"]]),
            },
            TestCase {
                name: "reverse sort",
                file_schema: Schema::new(vec![Field::new(
                    "value".to_string(),
                    DataType::Float64,
                    false,
                )]),
                files: vec![
                    File::new("0", "2023-01-01", vec![Some((0.00, 0.49))]),
                    File::new("1", "2023-01-01", vec![Some((0.50, 1.00))]),
                    File::new("2", "2023-01-02", vec![Some((0.00, 1.00))]),
                ],
                sort: vec![col("value").sort(false, true)],
                expected_result: Ok(vec![vec!["1", "0"], vec!["2"]]),
            },
            // reject nullable sort columns
            TestCase {
                name: "no nullable sort columns",
                file_schema: Schema::new(vec![Field::new(
                    "value".to_string(),
                    DataType::Float64,
                    true, // should fail because nullable
                )]),
                files: vec![
                    File::new("0", "2023-01-01", vec![Some((0.00, 0.49))]),
                    File::new("1", "2023-01-01", vec![Some((0.50, 1.00))]),
                    File::new("2", "2023-01-02", vec![Some((0.00, 1.00))]),
                ],
                sort: vec![col("value").sort(true, false)],
                expected_result: Err("construct min/max statistics for split_groups_by_statistics\ncaused by\nbuild min rows\ncaused by\ncreate sorting columns\ncaused by\nError during planning: cannot sort by nullable column")
            },
            TestCase {
                name: "all three non-overlapping",
                file_schema: Schema::new(vec![Field::new(
                    "value".to_string(),
                    DataType::Float64,
                    false,
                )]),
                files: vec![
                    File::new("0", "2023-01-01", vec![Some((0.00, 0.49))]),
                    File::new("1", "2023-01-01", vec![Some((0.50, 0.99))]),
                    File::new("2", "2023-01-02", vec![Some((1.00, 1.49))]),
                ],
                sort: vec![col("value").sort(true, false)],
                expected_result: Ok(vec![vec!["0", "1", "2"]]),
            },
            TestCase {
                name: "all three overlapping",
                file_schema: Schema::new(vec![Field::new(
                    "value".to_string(),
                    DataType::Float64,
                    false,
                )]),
                files: vec![
                    File::new("0", "2023-01-01", vec![Some((0.00, 0.49))]),
                    File::new("1", "2023-01-01", vec![Some((0.00, 0.49))]),
                    File::new("2", "2023-01-02", vec![Some((0.00, 0.49))]),
                ],
                sort: vec![col("value").sort(true, false)],
                expected_result: Ok(vec![vec!["0"], vec!["1"], vec!["2"]]),
            },
            TestCase {
                name: "empty input",
                file_schema: Schema::new(vec![Field::new(
                    "value".to_string(),
                    DataType::Float64,
                    false,
                )]),
                files: vec![],
                sort: vec![col("value").sort(true, false)],
                expected_result: Ok(vec![]),
            },
            TestCase {
                name: "one file missing statistics",
                file_schema: Schema::new(vec![Field::new(
                    "value".to_string(),
                    DataType::Float64,
                    false,
                )]),
                files: vec![
                    File::new("0", "2023-01-01", vec![Some((0.00, 0.49))]),
                    File::new("1", "2023-01-01", vec![Some((0.00, 0.49))]),
                    File::new("2", "2023-01-02", vec![None]),
                ],
                sort: vec![col("value").sort(true, false)],
                expected_result: Err("construct min/max statistics for split_groups_by_statistics\ncaused by\ncollect min/max values\ncaused by\nget min/max for column: 'value'\ncaused by\nError during planning: statistics not found"),
            },
        ];

        for case in cases {
            let table_schema = Arc::new(Schema::new(
                case.file_schema
                    .fields()
                    .clone()
                    .into_iter()
                    .cloned()
                    .chain(Some(Arc::new(Field::new(
                        "date".to_string(),
                        DataType::Utf8,
                        false,
                    ))))
                    .collect::<Vec<_>>(),
            ));
            let sort_order = LexOrdering::from(
                case.sort
                    .into_iter()
                    .map(|expr| {
                        crate::physical_planner::create_physical_sort_expr(
                            &expr,
                            &DFSchema::try_from(table_schema.as_ref().clone())?,
                            &ExecutionProps::default(),
                        )
                    })
                    .collect::<Result<Vec<_>>>()?,
            );

            let partitioned_files =
                case.files.into_iter().map(From::from).collect::<Vec<_>>();
            let result = FileScanConfig::split_groups_by_statistics(
                &table_schema,
                &[partitioned_files.clone()],
                &sort_order,
            );
            let results_by_name = result
                .as_ref()
                .map(|file_groups| {
                    file_groups
                        .iter()
                        .map(|file_group| {
                            file_group
                                .iter()
                                .map(|file| {
                                    partitioned_files
                                        .iter()
                                        .find_map(|f| {
                                            if f.object_meta == file.object_meta {
                                                Some(
                                                    f.object_meta
                                                        .location
                                                        .as_ref()
                                                        .rsplit('/')
                                                        .next()
                                                        .unwrap()
                                                        .trim_end_matches(".parquet"),
                                                )
                                            } else {
                                                None
                                            }
                                        })
                                        .unwrap()
                                })
                                .collect::<Vec<_>>()
                        })
                        .collect::<Vec<_>>()
                })
                .map_err(|e| e.strip_backtrace().leak() as &'static str);

            assert_eq!(results_by_name, case.expected_result, "{}", case.name);
        }

        return Ok(());

        impl From<File> for PartitionedFile {
            fn from(file: File) -> Self {
                PartitionedFile {
                    object_meta: ObjectMeta {
                        location: Path::from(format!(
                            "data/date={}/{}.parquet",
                            file.date, file.name
                        )),
                        last_modified: chrono::Utc.timestamp_nanos(0),
                        size: 0,
                        e_tag: None,
                        version: None,
                    },
                    partition_values: vec![ScalarValue::from(file.date)],
                    range: None,
                    statistics: Some(Statistics {
                        num_rows: Precision::Absent,
                        total_byte_size: Precision::Absent,
                        column_statistics: file
                            .statistics
                            .into_iter()
                            .map(|stats| {
                                stats
                                    .map(|(min, max)| ColumnStatistics {
                                        min_value: Precision::Exact(ScalarValue::from(
                                            min,
                                        )),
                                        max_value: Precision::Exact(ScalarValue::from(
                                            max,
                                        )),
                                        ..Default::default()
                                    })
                                    .unwrap_or_default()
                            })
                            .collect::<Vec<_>>(),
                    }),
                    extensions: None,
                    metadata_size_hint: None,
                }
            }
        }
    }

    // sets default for configs that play no role in projections
    fn config_for_projection(
        file_schema: SchemaRef,
        projection: Option<Vec<usize>>,
        statistics: Statistics,
        table_partition_cols: Vec<Field>,
    ) -> FileScanConfig {
        FileScanConfig::new(
            ObjectStoreUrl::parse("test:///").unwrap(),
            file_schema,
            Arc::new(ArrowSource::default()),
        )
        .with_projection(projection)
        .with_statistics(statistics)
        .with_table_partition_cols(table_partition_cols)
    }

    /// Convert partition columns from Vec<String DataType> to Vec<Field>
    fn to_partition_cols(table_partition_cols: Vec<(String, DataType)>) -> Vec<Field> {
        table_partition_cols
            .iter()
            .map(|(name, dtype)| Field::new(name, dtype.clone(), false))
            .collect::<Vec<_>>()
    }

    /// returns record batch with 3 columns of i32 in memory
    pub fn build_table_i32(
        a: (&str, &Vec<i32>),
        b: (&str, &Vec<i32>),
        c: (&str, &Vec<i32>),
    ) -> RecordBatch {
        let schema = Schema::new(vec![
            Field::new(a.0, DataType::Int32, false),
            Field::new(b.0, DataType::Int32, false),
            Field::new(c.0, DataType::Int32, false),
        ]);

        RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(Int32Array::from(a.1.clone())),
                Arc::new(Int32Array::from(b.1.clone())),
                Arc::new(Int32Array::from(c.1.clone())),
            ],
        )
        .unwrap()
    }
}
