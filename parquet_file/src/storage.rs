/// This module responsible to write given data to specify object store and
/// read them back
use arrow::{
    datatypes::{Schema, SchemaRef},
    error::{ArrowError, Result as ArrowResult},
    record_batch::RecordBatch,
};
use datafusion::{
    logical_plan::Expr,
    physical_plan::{
        parquet::ParquetExec, ExecutionPlan, Partitioning, RecordBatchStream,
        SendableRecordBatchStream,
    },
};
use internal_types::selection::Selection;
use object_store::{
    path::{parsed::DirsAndFileName, ObjectStorePath, Path},
    ObjectStore, ObjectStoreApi,
};
use observability_deps::tracing::debug;
use parquet::{
    self,
    arrow::ArrowWriter,
    file::{
        metadata::{KeyValue, ParquetMetaData},
        properties::WriterProperties,
        writer::TryClone,
    },
};
use query::predicate::Predicate;

use bytes::Bytes;
use data_types::server_id::ServerId;
use futures::{Stream, StreamExt};
use parking_lot::Mutex;
use snafu::{ensure, OptionExt, ResultExt, Snafu};
use std::{
    io::{Cursor, Seek, SeekFrom, Write},
    sync::Arc,
    task::{Context, Poll},
};
use tokio_stream::wrappers::ReceiverStream;

use crate::metadata::{read_parquet_metadata_from_file, IoxMetadata, METADATA_KEY};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Error opening Parquet Writer: {}", source))]
    OpeningParquetWriter {
        source: parquet::errors::ParquetError,
    },

    #[snafu(display("Error reading stream while creating snapshot: {}", source))]
    ReadingStream { source: ArrowError },

    #[snafu(display("Error writing Parquet to memory: {}", source))]
    WritingParquetToMemory {
        source: parquet::errors::ParquetError,
    },

    #[snafu(display("Error closing Parquet Writer: {}", source))]
    ClosingParquetWriter {
        source: parquet::errors::ParquetError,
    },

    #[snafu(display("Error writing to object store: {}", source))]
    WritingToObjectStore { source: object_store::Error },

    #[snafu(display("Error converting to vec[u8]: Nothing else should have a reference here"))]
    WritingToMemWriter {},

    #[snafu(display("Non local file not supported"))]
    NonLocalFile {},

    #[snafu(display("Error opening file: {}", source))]
    OpenFile { source: std::io::Error },

    #[snafu(display("Error opening temp file: {}", source))]
    OpenTempFile { source: std::io::Error },

    #[snafu(display("Error writing to temp file: {}", source))]
    WriteTempFile { source: std::io::Error },

    #[snafu(display("Internal error: can not get temp file as str: {}", path))]
    TempFilePathAsStr { path: String },

    #[snafu(display("Error creating parquet reader: {}", source))]
    CreatingParquetReader {
        source: datafusion::error::DataFusionError,
    },

    #[snafu(display(
        "Internal error: unexpected partitioning in parquet reader: {:?}",
        partitioning
    ))]
    UnexpectedPartitioning { partitioning: Partitioning },

    #[snafu(display("Error creating pruning predicate: {}", source))]
    CreatingPredicate {
        source: datafusion::error::DataFusionError,
    },

    #[snafu(display("Error reading from parquet stream: {}", source))]
    ReadingParquet {
        source: datafusion::error::DataFusionError,
    },

    #[snafu(display("Error at serialized file reader: {}", source))]
    SerializedFileReaderError {
        source: parquet::errors::ParquetError,
    },

    #[snafu(display("Error at parquet arrow reader: {}", source))]
    ParquetArrowReaderError {
        source: parquet::errors::ParquetError,
    },

    #[snafu(display("Error reading data from parquet file: {}", source))]
    ReadingFile { source: ArrowError },

    #[snafu(display("Error reading data from object store: {}", source))]
    ReadingObjectStore { source: object_store::Error },

    #[snafu(display("Error sending results: {}", source))]
    SendResult {
        source: datafusion::error::DataFusionError,
    },

    #[snafu(display("Cannot extract Parquet metadata from byte array: {}", source))]
    ExtractingMetadataFailure { source: crate::metadata::Error },

    #[snafu(display("Cannot parse location: {:?}", path))]
    LocationParsingFailure { path: DirsAndFileName },

    #[snafu(display("Cannot encode metadata: {}", source))]
    MetadataEncodeFailure { source: serde_json::Error },
}
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug)]
pub struct ParquetStream {
    schema: SchemaRef,
    inner: ReceiverStream<ArrowResult<RecordBatch>>,
}

impl Stream for ParquetStream {
    type Item = ArrowResult<RecordBatch>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(cx)
    }
}

impl RecordBatchStream for ParquetStream {
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }
}

#[derive(Debug, Clone)]
pub struct Storage {
    object_store: Arc<ObjectStore>,
    server_id: ServerId,
    db_name: String,
}

impl Storage {
    pub fn new(
        object_store: Arc<ObjectStore>,
        server_id: ServerId,
        db_name: impl Into<String>,
    ) -> Self {
        let db_name = db_name.into();
        Self {
            object_store,
            server_id,
            db_name,
        }
    }

    /// Return full path including filename in the object store to save a chunk
    /// table file.
    ///
    /// See [`parse_location`](Self::parse_location) for parsing.
    pub fn location(
        &self,
        partition_key: String,
        chunk_id: u32,
        table_name: String,
    ) -> object_store::path::Path {
        // Full path of the file in object store
        //    <server id>/<database>/data/<partition key>/<chunk id>/<table
        // name>.parquet

        let mut path = data_location(&self.object_store, self.server_id, &self.db_name);
        path.push_dir(partition_key);
        path.push_dir(chunk_id.to_string());
        let file_name = format!("{}.parquet", table_name);
        path.set_file_name(file_name);

        path
    }

    /// Parse locations and return partition key, chunk ID and table name.
    ///
    /// See [`location`](Self::location) for path generation.
    pub fn parse_location(
        &self,
        path: impl Into<DirsAndFileName>,
    ) -> Result<(String, u32, String)> {
        let path: DirsAndFileName = path.into();

        let dirs: Vec<_> = path.directories.iter().map(|part| part.encoded()).collect();
        match (dirs.as_slice(), &path.file_name) {
            ([server_id, db_name, "data", partition_key, chunk_id], Some(filename))
                if (server_id == &self.server_id.to_string()) && (db_name == &self.db_name) =>
            {
                let chunk_id: u32 = match chunk_id.parse() {
                    Ok(x) => x,
                    Err(_) => return Err(Error::LocationParsingFailure { path }),
                };

                let parts: Vec<_> = filename.encoded().split('.').collect();
                let table_name = match parts[..] {
                    [name, "parquet"] => name,
                    _ => return Err(Error::LocationParsingFailure { path }),
                };

                Ok((partition_key.to_string(), chunk_id, table_name.to_string()))
            }
            _ => Err(Error::LocationParsingFailure { path }),
        }
    }

    /// Write the given stream of data of a specified table of
    /// a specified partitioned chunk to a parquet file of this storage
    pub async fn write_to_object_store(
        &self,
        partition_key: String,
        chunk_id: u32,
        table_name: String,
        stream: SendableRecordBatchStream,
        metadata: IoxMetadata,
    ) -> Result<(Path, ParquetMetaData)> {
        // Create full path location of this file in object store
        let path = self.location(partition_key, chunk_id, table_name);

        let schema = stream.schema();
        let data = Self::parquet_stream_to_bytes(stream, schema, metadata).await?;
        // TODO: make this work w/o cloning the byte vector (https://github.com/influxdata/influxdb_iox/issues/1504)
        let md =
            read_parquet_metadata_from_file(data.clone()).context(ExtractingMetadataFailure)?;
        self.to_object_store(data, &path).await?;

        Ok((path.clone(), md))
    }

    /// Convert the given stream of RecordBatches to bytes
    async fn parquet_stream_to_bytes(
        mut stream: SendableRecordBatchStream,
        schema: SchemaRef,
        metadata: IoxMetadata,
    ) -> Result<Vec<u8>> {
        let props = WriterProperties::builder()
            .set_key_value_metadata(Some(vec![KeyValue {
                key: METADATA_KEY.to_string(),
                value: Some(serde_json::to_string(&metadata).context(MetadataEncodeFailure)?),
            }]))
            .build();

        let mem_writer = MemWriter::default();
        {
            let mut writer = ArrowWriter::try_new(mem_writer.clone(), schema, Some(props))
                .context(OpeningParquetWriter)?;
            while let Some(batch) = stream.next().await {
                let batch = batch.context(ReadingStream)?;
                writer.write(&batch).context(WritingParquetToMemory)?;
            }
            writer.close().context(ClosingParquetWriter)?;
        } // drop the reference to the MemWriter that the SerializedFileWriter has

        mem_writer.into_inner().context(WritingToMemWriter)
    }

    /// Put the given vector of bytes to the specified location
    pub async fn to_object_store(
        &self,
        data: Vec<u8>,
        file_name: &object_store::path::Path,
    ) -> Result<()> {
        let len = data.len();
        let data = Bytes::from(data);
        let stream_data = Result::Ok(data);

        self.object_store
            .put(
                &file_name,
                futures::stream::once(async move { stream_data }),
                Some(len),
            )
            .await
            .context(WritingToObjectStore)
    }

    /// Return indices of the schema's fields of the selection columns
    pub fn column_indices(selection: Selection<'_>, schema: SchemaRef) -> Vec<usize> {
        let fields = schema.fields().iter();

        match selection {
            Selection::Some(cols) => fields
                .enumerate()
                .filter_map(|(p, x)| {
                    if cols.contains(&x.name().as_str()) {
                        Some(p)
                    } else {
                        None
                    }
                })
                .collect(),
            Selection::All => fields.enumerate().map(|(p, _)| p).collect(),
        }
    }

    /// Downloads the specified parquet file to a local temporary file
    /// and uses the `[ParquetExec`] from DataFusion to read that
    /// parquet file (including predicate and projection pushdown).
    ///
    /// The resulting record batches from Parquet are sent back to `tx`
    async fn download_and_scan_parquet(
        predicate: Option<Expr>,
        projection: Vec<usize>,
        path: Path,
        store: Arc<ObjectStore>,
        tx: tokio::sync::mpsc::Sender<ArrowResult<RecordBatch>>,
    ) -> Result<()> {
        // Size of each batch
        let batch_size = 1024; // Todo: make a constant or policy for this
        let max_concurrency = 1; // Todo: make a constant or policy for this

        // Limit of total rows to read
        let limit: Option<usize> = None; // Todo: this should be a parameter of the function

        // read parquet file to local file
        let mut temp_file = tempfile::Builder::new()
            .prefix("iox-parquet-cache")
            .suffix(".parquet")
            .tempfile()
            .context(OpenTempFile)?;

        debug!(?path, ?temp_file, "Beginning to read parquet to temp file");
        let mut read_stream = store.get(&path).await.context(ReadingObjectStore)?;

        while let Some(bytes) = read_stream.next().await {
            let bytes = bytes.context(ReadingObjectStore)?;
            debug!(len = bytes.len(), "read bytes from object store");
            temp_file.write_all(&bytes).context(WriteTempFile)?;
        }

        // now, create the appropriate parquet exec from datafusion and make it
        let temp_path = temp_file.into_temp_path();
        debug!(?temp_path, "Completed read parquet to tempfile");

        let temp_path = temp_path.to_str().with_context(|| TempFilePathAsStr {
            path: temp_path.to_string_lossy(),
        })?;

        let parquet_exec = ParquetExec::try_from_path(
            temp_path,
            Some(projection),
            predicate,
            batch_size,
            max_concurrency,
            limit,
        )
        .context(CreatingParquetReader)?;

        // We are assuming there is only a single stream in the
        // call to execute(0) below
        let partitioning = parquet_exec.output_partitioning();
        ensure!(
            matches!(partitioning, Partitioning::UnknownPartitioning(1)),
            UnexpectedPartitioning { partitioning }
        );

        let mut parquet_stream = parquet_exec.execute(0).await.context(ReadingParquet)?;

        while let Some(batch) = parquet_stream.next().await {
            if let Err(e) = tx.send(batch).await {
                debug!(%e, "Stopping parquet exec early, receiver hung up");
                return Ok(());
            }
        }
        Ok(())
    }

    pub fn read_filter(
        predicate: &Predicate,
        selection: Selection<'_>,
        schema: SchemaRef,
        path: Path,
        store: Arc<ObjectStore>,
    ) -> Result<SendableRecordBatchStream> {
        // fire up a async task that will fetch the parquet file
        // locally, start it executing and send results

        // Indices of columns in the schema needed to read
        let projection: Vec<usize> = Self::column_indices(selection, Arc::clone(&schema));

        // Compute final (output) schema after selection
        let schema = Arc::new(Schema::new(
            projection
                .iter()
                .map(|i| schema.field(*i).clone())
                .collect(),
        ));

        // pushdown predicate, if any
        let predicate = predicate.filter_expr();

        let (tx, rx) = tokio::sync::mpsc::channel(2);

        // Run async dance here to make sure any error returned
        // `download_and_scan_parquet` is sent back to the reader and
        // not silently ignored
        tokio::task::spawn(async move {
            let download_result =
                Self::download_and_scan_parquet(predicate, projection, path, store, tx.clone())
                    .await;

            // If there was an error returned from download_and_scan_parquet send it back to the receiver.
            if let Err(e) = download_result {
                let e = ArrowError::ExternalError(Box::new(e));
                if let Err(e) = tx.send(ArrowResult::Err(e)).await {
                    // if no one is listening, there is no one else to hear our screams
                    debug!(%e, "Error sending result of download function. Receiver is closed.");
                }
            }
        });

        // returned stream simply reads off the rx stream
        let stream = ParquetStream {
            schema,
            inner: ReceiverStream::new(rx),
        };

        Ok(Box::pin(stream))
    }
}

#[derive(Debug, Default, Clone)]
pub struct MemWriter {
    mem: Arc<Mutex<Cursor<Vec<u8>>>>,
}

impl MemWriter {
    /// Returns the inner buffer as long as there are no other references to the
    /// Arc.
    pub fn into_inner(self) -> Option<Vec<u8>> {
        Arc::try_unwrap(self.mem)
            .ok()
            .map(|mutex| mutex.into_inner().into_inner())
    }
}

impl Write for MemWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut inner = self.mem.lock();
        inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut inner = self.mem.lock();
        inner.flush()
    }
}

impl Seek for MemWriter {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let mut inner = self.mem.lock();
        inner.seek(pos)
    }
}

impl TryClone for MemWriter {
    fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self {
            mem: Arc::clone(&self.mem),
        })
    }
}

/// Location where parquet data goes to.
///
/// Schema currently is:
///
/// ```text
/// <writer_id>/<database>/data
/// ```
pub(crate) fn data_location(
    object_store: &ObjectStore,
    server_id: ServerId,
    db_name: &str,
) -> Path {
    let mut path = object_store.new_path();
    path.push_dir(server_id.to_string());
    path.push_dir(db_name.to_string());
    path.push_dir("data");
    path
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use crate::test_utils::{make_object_store, make_record_batch};
    use arrow::array::{ArrayRef, StringArray};
    use arrow_util::assert_batches_eq;
    use datafusion::physical_plan::common::SizedRecordBatchStream;
    use datafusion_util::MemoryStream;
    use object_store::parsed_path;
    use uuid::Uuid;

    #[tokio::test]
    async fn test_parquet_contains_key_value_metadata() {
        let metadata = IoxMetadata {
            transaction_revision_counter: 42,
            transaction_uuid: Uuid::new_v4(),
        };

        // create parquet file
        let (_record_batches, schema, _column_summaries, _num_rows) = make_record_batch("foo");
        let stream: SendableRecordBatchStream = Box::pin(MemoryStream::new_with_schema(
            vec![],
            Arc::clone(schema.inner()),
        ));
        let bytes =
            Storage::parquet_stream_to_bytes(stream, Arc::clone(schema.inner()), metadata.clone())
                .await
                .unwrap();

        // extract metadata
        let md = read_parquet_metadata_from_file(bytes).unwrap();
        let kv_vec = md.file_metadata().key_value_metadata().as_ref().unwrap();

        // filter out relevant key
        let kv = kv_vec
            .iter()
            .find(|kv| kv.key == METADATA_KEY)
            .cloned()
            .unwrap();

        // compare with input
        let metadata_roundtrip: IoxMetadata = serde_json::from_str(&kv.value.unwrap()).unwrap();
        assert_eq!(metadata_roundtrip, metadata);
    }

    #[test]
    fn test_location_to_from_path() {
        let server_id = ServerId::new(NonZeroU32::new(1).unwrap());
        let store = Storage::new(make_object_store(), server_id, "my_db");

        // happy roundtrip
        let path = store.location("p1".to_string(), 42, "my_table".to_string());
        assert_eq!(path.display(), "1/my_db/data/p1/42/my_table.parquet");
        assert_eq!(
            store.parse_location(path).unwrap(),
            ("p1".to_string(), 42, "my_table".to_string())
        );

        // error cases
        assert!(store.parse_location(parsed_path!()).is_err());
        assert!(store
            .parse_location(parsed_path!(["too", "short"], "my_table.parquet"))
            .is_err());
        assert!(store
            .parse_location(parsed_path!(
                ["this", "is", "way", "way", "too", "long"],
                "my_table.parquet"
            ))
            .is_err());
        assert!(store
            .parse_location(parsed_path!(
                ["1", "my_db", "data", "p1", "not_a_number"],
                "my_table.parquet"
            ))
            .is_err());
        assert!(store
            .parse_location(parsed_path!(
                ["1", "my_db", "not_data", "p1", "42"],
                "my_table.parquet"
            ))
            .is_err());
        assert!(store
            .parse_location(parsed_path!(
                ["1", "other_db", "data", "p1", "42"],
                "my_table.parquet"
            ))
            .is_err());
        assert!(store
            .parse_location(parsed_path!(
                ["2", "my_db", "data", "p1", "42"],
                "my_table.parquet"
            ))
            .is_err());
        assert!(store
            .parse_location(parsed_path!(["1", "my_db", "data", "p1", "42"], "my_table"))
            .is_err());
        assert!(store
            .parse_location(parsed_path!(
                ["1", "my_db", "data", "p1", "42"],
                "my_table.parquet.tmp"
            ))
            .is_err());
    }

    #[tokio::test]
    async fn test_roundtrip() {
        test_helpers::maybe_start_logging();
        // validates that the async plubing is setup to read parquet files from object store

        // prepare input
        let array = StringArray::from(vec!["foo", "bar", "baz"]);
        let batch = RecordBatch::try_from_iter(vec![(
            "my_awesome_test_column",
            Arc::new(array) as ArrayRef,
        )])
        .unwrap();

        let expected = vec![
            "+------------------------+",
            "| my_awesome_test_column |",
            "+------------------------+",
            "| foo                    |",
            "| bar                    |",
            "| baz                    |",
            "+------------------------+",
        ];

        let input_batches = vec![batch.clone()];
        assert_batches_eq!(&expected, &input_batches);

        // create Storage
        let server_id = ServerId::new(NonZeroU32::new(1).unwrap());
        let storage = Storage::new(make_object_store(), server_id, "my_db");

        // write the data in
        let schema = batch.schema();
        let input_stream = Box::pin(SizedRecordBatchStream::new(
            batch.schema(),
            vec![Arc::new(batch)],
        ));
        let metadata = IoxMetadata {
            transaction_revision_counter: 42,
            transaction_uuid: Uuid::new_v4(),
        };

        let (path, _) = storage
            .write_to_object_store(
                "my_partition".to_string(),
                33,
                "my_table".to_string(),
                input_stream,
                metadata,
            )
            .await
            .expect("successfully wrote to object store");

        let object_store = Arc::clone(&storage.object_store);
        let read_stream = Storage::read_filter(
            &Predicate::default(),
            Selection::All,
            schema,
            path,
            object_store,
        )
        .expect("successfully called read_filter");

        let read_batches = datafusion::physical_plan::common::collect(read_stream)
            .await
            .expect("collecting results");

        assert_batches_eq!(&expected, &read_batches);
    }
}
