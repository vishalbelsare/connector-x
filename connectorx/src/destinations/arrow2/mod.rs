//! Destination implementation for Arrow2.

mod arrow_assoc;
mod errors;
mod funcs;
pub mod typesystem;

use super::{Consume, Destination, DestinationPartition};
use crate::constants::RECORD_BATCH_SIZE;
use crate::data_order::DataOrder;
use crate::typesystem::{Realize, TypeAssoc, TypeSystem};
use anyhow::anyhow;
use arrow2::array::MutableArray;
use arrow2::datatypes::Schema;
use arrow2::record_batch::RecordBatch;
use arrow_assoc::ArrowAssoc;
pub use errors::{Arrow2DestinationError, Result};
use fehler::throw;
use fehler::throws;
use funcs::{FFinishBuilder, FNewBuilder, FNewField};
use polars::frame::DataFrame;
use std::convert::TryFrom;
use std::sync::{Arc, Mutex};
pub use typesystem::Arrow2TypeSystem;

type Builder = Box<dyn MutableArray + 'static + Send>;
type Builders = Vec<Builder>;

pub struct Arrow2Destination {
    schema: Vec<Arrow2TypeSystem>,
    names: Vec<String>,
    data: Arc<Mutex<Vec<RecordBatch>>>,
    arrow_schema: Arc<Schema>,
}

impl Default for Arrow2Destination {
    fn default() -> Self {
        Arrow2Destination {
            schema: vec![],
            names: vec![],
            data: Arc::new(Mutex::new(vec![])),
            arrow_schema: Arc::new(Schema::empty()),
        }
    }
}

impl Arrow2Destination {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Destination for Arrow2Destination {
    const DATA_ORDERS: &'static [DataOrder] = &[DataOrder::ColumnMajor, DataOrder::RowMajor];
    type TypeSystem = Arrow2TypeSystem;
    type Partition<'a> = ArrowPartitionWriter;
    type Error = Arrow2DestinationError;

    fn needs_count(&self) -> bool {
        false
    }

    #[throws(Arrow2DestinationError)]
    fn allocate<S: AsRef<str>>(
        &mut self,
        _nrows: usize,
        names: &[S],
        schema: &[Arrow2TypeSystem],
        data_order: DataOrder,
    ) {
        // todo: support colmajor
        if !matches!(data_order, DataOrder::RowMajor) {
            throw!(crate::errors::ConnectorXError::UnsupportedDataOrder(
                data_order
            ))
        }

        // parse the metadata
        self.schema = schema.to_vec();
        self.names = names.iter().map(|n| n.as_ref().to_string()).collect();
        let fields = self
            .schema
            .iter()
            .zip(&self.names)
            .map(|(&dt, h)| Ok(Realize::<FNewField>::realize(dt)?(h.as_str())))
            .collect::<Result<Vec<_>>>()?;
        self.arrow_schema = Arc::new(Schema::new(fields));
    }

    #[throws(Arrow2DestinationError)]
    fn partition(&mut self, counts: usize) -> Vec<Self::Partition<'_>> {
        let mut partitions = vec![];
        for _ in 0..counts {
            partitions.push(ArrowPartitionWriter::new(
                self.schema.clone(),
                Arc::clone(&self.data),
                Arc::clone(&self.arrow_schema),
            )?);
        }
        partitions
    }

    fn schema(&self) -> &[Arrow2TypeSystem] {
        self.schema.as_slice()
    }
}

impl Arrow2Destination {
    #[throws(Arrow2DestinationError)]
    pub fn arrow(self) -> Vec<RecordBatch> {
        let lock = Arc::try_unwrap(self.data).map_err(|_| anyhow!("Partitions are not freed"))?;
        lock.into_inner()
            .map_err(|e| anyhow!("mutex poisoned {}", e))?
    }

    #[throws(Arrow2DestinationError)]
    pub fn polars(self) -> DataFrame {
        let rbs = self.arrow()?;
        DataFrame::try_from(rbs)?
    }
}

pub struct ArrowPartitionWriter {
    schema: Vec<Arrow2TypeSystem>,
    builders: Option<Builders>,
    current_row: usize,
    current_col: usize,
    data: Arc<Mutex<Vec<RecordBatch>>>,
    arrow_schema: Arc<Schema>,
}

impl ArrowPartitionWriter {
    #[throws(Arrow2DestinationError)]
    fn new(
        schema: Vec<Arrow2TypeSystem>,
        data: Arc<Mutex<Vec<RecordBatch>>>,
        arrow_schema: Arc<Schema>,
    ) -> Self {
        let mut pw = ArrowPartitionWriter {
            schema,
            builders: None,
            current_row: 0,
            current_col: 0,
            data,
            arrow_schema,
        };
        pw.allocate()?;
        pw
    }

    #[throws(Arrow2DestinationError)]
    fn allocate(&mut self) {
        let builders = self
            .schema
            .iter()
            .map(|&dt| Ok(Realize::<FNewBuilder>::realize(dt)?(RECORD_BATCH_SIZE)))
            .collect::<Result<Vec<_>>>()?;
        self.builders.replace(builders);
    }

    #[throws(Arrow2DestinationError)]
    fn flush(&mut self) {
        let builders = self
            .builders
            .take()
            .unwrap_or_else(|| panic!("arrow builder is none when flush!"));
        let columns = builders
            .into_iter()
            .zip(self.schema.iter())
            .map(|(builder, &dt)| Realize::<FFinishBuilder>::realize(dt)?(builder))
            .collect::<std::result::Result<Vec<_>, crate::errors::ConnectorXError>>()?;
        let rb = RecordBatch::try_new(Arc::clone(&self.arrow_schema), columns)?;
        {
            let mut guard = self
                .data
                .lock()
                .map_err(|e| anyhow!("mutex poisoned {}", e))?;
            let inner_data = &mut *guard;
            inner_data.push(rb);
        }
        self.current_row = 0;
        self.current_col = 0;
    }
}

impl<'a> DestinationPartition<'a> for ArrowPartitionWriter {
    type TypeSystem = Arrow2TypeSystem;
    type Error = Arrow2DestinationError;

    fn ncols(&self) -> usize {
        self.schema.len()
    }

    #[throws(Arrow2DestinationError)]
    fn finalize(&mut self) {
        if self.builders.is_some() {
            self.flush()?;
        }
    }

    #[throws(Arrow2DestinationError)]
    fn aquire_row(&mut self, _n: usize) -> usize {
        self.current_row
    }
}

impl<'a, T> Consume<T> for ArrowPartitionWriter
where
    T: TypeAssoc<<Self as DestinationPartition<'a>>::TypeSystem> + ArrowAssoc + 'static,
{
    type Error = Arrow2DestinationError;

    #[throws(Arrow2DestinationError)]
    fn consume(&mut self, value: T) {
        let col = self.current_col;
        self.current_col = (self.current_col + 1) % self.ncols();
        self.schema[col].check::<T>()?;

        match &mut self.builders {
            Some(builders) => {
                <T as ArrowAssoc>::push(
                    builders[col]
                        .as_mut_any()
                        .downcast_mut::<T::Builder>()
                        .ok_or_else(|| anyhow!("cannot cast arrow builder for append"))?,
                    value,
                );
            }
            None => throw!(anyhow!("arrow arrays are empty!")),
        }

        // flush if exceed batch_size
        if self.current_col == 0 {
            self.current_row += 1;
            if self.current_row >= RECORD_BATCH_SIZE {
                self.flush()?;
                self.allocate()?;
            }
        }
    }
}
