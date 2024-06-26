use std::pin::Pin;
use std::task::{ready, Context, Poll};

use arrow_array::RecordBatch;
use datafusion::arrow::datatypes::SchemaRef;
use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use models::arrow::stream::ParallelMergeStream;
use models::datafusion::limit_record_batch::limit_record_batch;

use crate::reader::{
    BatchReader, BatchReaderRef, SchemableTskvRecordBatchStream,
    SendableSchemableTskvRecordBatchStream,
};
use crate::{Error, Result};

pub struct ParallelMergeAdapter {
    schema: SchemaRef,
    inputs: Vec<BatchReaderRef>,
    limit: Option<usize>,
}

impl ParallelMergeAdapter {
    pub fn try_new(
        schema: SchemaRef,
        inputs: Vec<BatchReaderRef>,
        limit: Option<usize>,
    ) -> Result<Self> {
        if inputs.is_empty() {
            return Err(Error::CommonError {
                reason: "No inputs provided for ParallelMergeAdapter".to_string(),
            });
        }

        Ok(Self {
            schema,
            inputs,
            limit,
        })
    }
}

impl BatchReader for ParallelMergeAdapter {
    fn process(&self) -> Result<SendableSchemableTskvRecordBatchStream> {
        let streams = self
            .inputs
            .iter()
            .map(|e| -> Result<BoxStream<_>> { Ok(Box::pin(e.process()?)) })
            .collect::<Result<Vec<_>>>()?;

        let stream = ParallelMergeStream::new(None, streams);

        Ok(Box::pin(SchemableParallelMergeStream {
            schema: self.schema.clone(),
            stream,
            remain: self.limit,
        }))
    }

    fn fmt_as(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "ParallelMergeAdapter: limit={:#?}", self.limit)
    }

    fn children(&self) -> Vec<BatchReaderRef> {
        self.inputs.clone()
    }
}

pub struct SchemableParallelMergeStream {
    schema: SchemaRef,
    stream: ParallelMergeStream<Error>,
    remain: Option<usize>,
}

impl SchemableTskvRecordBatchStream for SchemableParallelMergeStream {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

impl Stream for SchemableParallelMergeStream {
    type Item = Result<RecordBatch>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        return match ready!(self.stream.poll_next_unpin(cx)) {
            Some(Ok(batch)) => Poll::Ready(limit_record_batch(self.remain.as_mut(), batch).map(Ok)),
            other => Poll::Ready(other),
        };
    }
}
