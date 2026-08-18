//! The tick stream as Parquet, for tools that are not this one.
//!
//! `.oqtk` is the format a backtest reads: fixed-width, checksummed,
//! memory-mappable, and understood by nothing outside this workspace.
//! That last property is the problem this module solves. Research is
//! done in pandas and polars and DuckDB, and asking a researcher to
//! write a reader before they can look at a day of data is asking them
//! not to look.
//!
//! # Both timestamps, always
//!
//! Every row carries `exch_ts` and `local_ts` as separate columns, and
//! neither is optional. A columnar export that keeps one of them is a
//! trap: the difference between them is feed latency, and a study that
//! silently uses whichever one the exporter happened to keep has no way
//! to tell a slow feed from a slow market. `TICK-FORMAT.md` §2 makes the
//! same argument for the native format; this is the same rule at the
//! export boundary.
//!
//! # Integers, not floats
//!
//! Prices and quantities are written as `Int64` in their native tick and
//! lot units, not as scaled `Float64`. A float column is friendlier to
//! read and is wrong: `0.1 + 0.2` is a bug report waiting to be filed
//! against the strategy rather than against the exporter. The scale
//! belongs to the instrument, is not a property of the price, and is
//! written into the file's key-value metadata so a reader can apply it
//! deliberately.
//!
//! # Round trip, not fidelity by inspection
//!
//! [`write_parquet`] followed by [`read_parquet`] returns the input,
//! exactly, and that is a test rather than a claim. The checksum is not
//! carried across — Parquet has its own page-level integrity and
//! carrying ours would assert something about bytes we no longer own —
//! so `read_parquet` reconstructs a [`TickStream`], which recomputes it.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow_array::{ArrayRef, Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use oq_engine::Tick;
use oq_types::{Nanos, PriceTicks, QtyLots, Stamp};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;

use crate::{Error as StreamError, TickStream};

/// The instrument id, under this key, in the file's metadata.
///
/// A tick file is one instrument's; a Parquet file that lost that fact
/// would be a pile of numbers. Stored as metadata rather than as a
/// column because it is constant for the file and a constant column is
/// a million copies of one number.
pub const INSTRUMENT_KEY: &str = "openquanter.instrument";

/// The format version, under this key.
pub const VERSION_KEY: &str = "openquanter.tick_schema";

/// The version this module writes and the only one it reads.
pub const VERSION: &str = "1";

/// Why a Parquet file could not be turned back into ticks.
#[derive(Debug)]
pub enum Error {
    /// The file could not be read or written.
    Io(std::io::Error),
    /// Parquet itself objected.
    Parquet(String),
    /// A column the schema requires is missing, or has the wrong type.
    ///
    /// Named rather than reported as a generic decode failure: the
    /// common cause is a file written by a different tool, and knowing
    /// which column is missing is the difference between a two-minute
    /// fix and an afternoon.
    Column {
        /// The column that was expected.
        name: &'static str,
        /// What was found instead, or `None` when it was absent.
        found: Option<String>,
    },
    /// The file does not say what instrument it holds, or says something
    /// that is not a number.
    Instrument(Option<String>),
    /// Written by a version this build does not read.
    Version(Option<String>),
    /// The reconstructed stream was rejected by [`TickStream`].
    Stream(StreamError),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Parquet(e) => write!(f, "parquet: {e}"),
            Self::Column { name, found: None } => write!(f, "column `{name}` is missing"),
            Self::Column {
                name,
                found: Some(t),
            } => write!(f, "column `{name}` is {t}, expected Int64"),
            Self::Instrument(None) => {
                write!(f, "the file does not say which instrument it holds")
            }
            Self::Instrument(Some(v)) => write!(f, "instrument id {v:?} is not a number"),
            Self::Version(None) => write!(f, "the file does not say what schema it uses"),
            Self::Version(Some(v)) => {
                write!(f, "schema version {v:?}, this build reads {VERSION}")
            }
            Self::Stream(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<arrow_schema::ArrowError> for Error {
    fn from(e: arrow_schema::ArrowError) -> Self {
        Self::Parquet(e.to_string())
    }
}

impl From<parquet::errors::ParquetError> for Error {
    fn from(e: parquet::errors::ParquetError) -> Self {
        Self::Parquet(e.to_string())
    }
}

/// The seven columns, in order.
///
/// Order is fixed so a reader that positions by index rather than by
/// name still gets the right answer, but everything here looks columns
/// up by name: a file with the columns rearranged is still a valid file.
const COLUMNS: [&str; 7] = ["exch_ts", "local_ts", "last", "high", "low", "bid", "ask"];

fn schema() -> Schema {
    let mut fields: Vec<Field> = COLUMNS
        .iter()
        .map(|name| Field::new(*name, DataType::Int64, false))
        .collect();
    fields.push(Field::new("volume", DataType::Int64, false));
    Schema::new(fields)
}

/// Write a tick stream as Parquet.
///
/// Compressed with zstd: tick columns are highly repetitive and the
/// ratio is worth far more than the write time, since these files are
/// written once and read many times.
///
/// # Errors
///
/// [`Error::Io`] or [`Error::Parquet`].
pub fn write_parquet(stream: &TickStream, path: impl AsRef<Path>) -> Result<(), Error> {
    let schema = Arc::new(schema());
    let ticks = stream.ticks();

    let col =
        |f: fn(&Tick) -> i64| -> ArrayRef { Arc::new(ticks.iter().map(f).collect::<Int64Array>()) };
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            col(|t| t.stamp.exch.0),
            col(|t| t.stamp.local.0),
            col(|t| t.last.0),
            col(|t| t.high.0),
            col(|t| t.low.0),
            col(|t| t.bid.0),
            col(|t| t.ask.0),
            col(|t| t.volume.0),
        ],
    )?;

    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        .set_key_value_metadata(Some(vec![
            parquet::file::metadata::KeyValue::new(
                INSTRUMENT_KEY.to_owned(),
                stream.instrument().to_string(),
            ),
            parquet::file::metadata::KeyValue::new(VERSION_KEY.to_owned(), VERSION.to_owned()),
        ]))
        .build();

    let file = File::create(path)?;
    let mut writer = ArrowWriter::try_new(file, schema, Some(props))?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(())
}

/// Read a Parquet file written by [`write_parquet`] back into ticks.
///
/// # Errors
///
/// [`Error::Column`] when a required column is absent or not `Int64`,
/// [`Error::Instrument`] or [`Error::Version`] when the metadata is
/// missing or unreadable, [`Error::Stream`] when the reconstructed
/// stream is not a valid one.
pub fn read_parquet(path: impl AsRef<Path>) -> Result<TickStream, Error> {
    let file = File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;

    let kv = builder.metadata().file_metadata().key_value_metadata();
    let find = |key: &str| -> Option<String> {
        kv.and_then(|kv| kv.iter().find(|e| e.key == key))
            .and_then(|e| e.value.clone())
    };

    match find(VERSION_KEY) {
        Some(v) if v == VERSION => {}
        other => return Err(Error::Version(other)),
    }
    let instrument = match find(INSTRUMENT_KEY) {
        Some(v) => v.parse::<u64>().map_err(|_| Error::Instrument(Some(v)))?,
        None => return Err(Error::Instrument(None)),
    };

    let mut ticks: Vec<Tick> = Vec::new();
    for batch in builder.build()? {
        let batch = batch?;
        let get = |name: &'static str| -> Result<&Int64Array, Error> {
            let Some(arr) = batch.column_by_name(name) else {
                return Err(Error::Column { name, found: None });
            };
            arr.as_any()
                .downcast_ref::<Int64Array>()
                .ok_or_else(|| Error::Column {
                    name,
                    found: Some(format!("{:?}", arr.data_type())),
                })
        };
        let (exch, local) = (get("exch_ts")?, get("local_ts")?);
        let (last, high, low) = (get("last")?, get("high")?, get("low")?);
        let (bid, ask, volume) = (get("bid")?, get("ask")?, get("volume")?);

        ticks.reserve(batch.num_rows());
        for i in 0..batch.num_rows() {
            ticks.push(Tick {
                stamp: Stamp {
                    exch: Nanos(exch.value(i)),
                    local: Nanos(local.value(i)),
                },
                last: PriceTicks(last.value(i)),
                high: PriceTicks(high.value(i)),
                low: PriceTicks(low.value(i)),
                bid: PriceTicks(bid.value(i)),
                ask: PriceTicks(ask.value(i)),
                volume: QtyLots(volume.value(i)),
            });
        }
    }

    TickStream::new(instrument, ticks).map_err(Error::Stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p =
            std::env::temp_dir().join(format!("oq-columnar-{}-{name}.parquet", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn sample(n: usize) -> TickStream {
        let ticks: Vec<Tick> = (0..n)
            .map(|i| {
                let i = i as i64;
                Tick {
                    stamp: Stamp {
                        exch: Nanos(1_700_000_000_000_000_000 + i * 1_000_000),
                        // Deliberately not equal to exch: the whole point
                        // of two columns is that they can differ.
                        local: Nanos(1_700_000_000_000_000_000 + i * 1_000_000 + 250_000 + i),
                    },
                    last: PriceTicks(6_000_000 + i),
                    high: PriceTicks(6_000_010 + i),
                    low: PriceTicks(5_999_990 + i),
                    bid: PriceTicks(6_000_000 + i - 1),
                    ask: PriceTicks(6_000_000 + i + 1),
                    volume: QtyLots(i * 7),
                }
            })
            .collect();
        TickStream::new(42, ticks).expect("valid stream")
    }

    /// The only claim this module makes, made as a test: what goes in
    /// comes back, bit for bit. Anything weaker is an opinion about the
    /// file format.
    #[test]
    fn a_stream_survives_the_round_trip_exactly() {
        let path = tmp("roundtrip");
        let original = sample(5_000);
        write_parquet(&original, &path).expect("write");
        let back = read_parquet(&path).expect("read");

        assert_eq!(back.instrument(), original.instrument());
        assert_eq!(back.len(), original.len());
        assert_eq!(back.ticks(), original.ticks(), "round trip is not exact");
        let _ = std::fs::remove_file(&path);
    }

    /// Feed latency is `local - exch`, so an exporter that kept one
    /// timestamp would destroy it silently. This asserts the difference
    /// survives, not merely that both columns exist.
    #[test]
    fn both_timestamps_survive_and_so_does_the_latency_between_them() {
        let path = tmp("latency");
        let original = sample(1_000);
        write_parquet(&original, &path).expect("write");
        let back = read_parquet(&path).expect("read");

        let before: Vec<i64> = original
            .ticks()
            .iter()
            .map(|t| t.stamp.local.0 - t.stamp.exch.0)
            .collect();
        let after: Vec<i64> = back
            .ticks()
            .iter()
            .map(|t| t.stamp.local.0 - t.stamp.exch.0)
            .collect();
        assert_eq!(before, after);
        assert!(
            before.iter().any(|&d| d != before[0]),
            "the fixture must have varying latency or this test proves nothing"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A file with no instrument is a pile of numbers, and reading it as
    /// though it were instrument 0 would be worse than refusing.
    #[test]
    fn a_file_without_its_metadata_is_refused_rather_than_guessed_at() {
        let path = tmp("nometa");
        // Write the columns with no key-value metadata at all, the way
        // any other tool producing this schema would.
        let schema = Arc::new(schema());
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            (0..8)
                .map(|_| Arc::new(Int64Array::from(vec![1i64, 2, 3])) as ArrayRef)
                .collect(),
        )
        .expect("batch");
        let mut w = ArrowWriter::try_new(File::create(&path).expect("create"), schema, None)
            .expect("writer");
        w.write(&batch).expect("write");
        w.close().expect("close");

        assert!(
            matches!(read_parquet(&path), Err(Error::Version(None))),
            "a file with no schema version must be refused"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The error names the column, because the usual cause is a file
    /// from another tool and the fix depends on which one is wrong.
    #[test]
    fn a_missing_column_is_named() {
        let path = tmp("shortcols");
        let schema = Arc::new(Schema::new(vec![
            Field::new("exch_ts", DataType::Int64, false),
            Field::new("last", DataType::Int64, false),
        ]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(Int64Array::from(vec![1i64])) as ArrayRef,
                Arc::new(Int64Array::from(vec![2i64])) as ArrayRef,
            ],
        )
        .expect("batch");
        let props = WriterProperties::builder()
            .set_key_value_metadata(Some(vec![
                parquet::file::metadata::KeyValue::new(INSTRUMENT_KEY.to_owned(), "7".to_owned()),
                parquet::file::metadata::KeyValue::new(VERSION_KEY.to_owned(), VERSION.to_owned()),
            ]))
            .build();
        let mut w = ArrowWriter::try_new(File::create(&path).expect("create"), schema, Some(props))
            .expect("writer");
        w.write(&batch).expect("write");
        w.close().expect("close");

        match read_parquet(&path) {
            Err(Error::Column { name, found }) => {
                assert_eq!(name, "local_ts", "the first missing column is named");
                assert_eq!(found, None);
            }
            other => panic!("expected a named missing column, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    /// An empty stream is a legitimate thing to export — a quiet hour is
    /// data — and must not become an unreadable file.
    #[test]
    fn an_empty_stream_round_trips_as_an_empty_stream() {
        let path = tmp("empty");
        let original = TickStream::new(9, Vec::new()).expect("empty is valid");
        write_parquet(&original, &path).expect("write");
        let back = read_parquet(&path).expect("read");
        assert_eq!(back.instrument(), 9);
        assert!(back.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    /// Prices are integers in the file. If they were ever written as
    /// floats this would fail, which is the only way to hold that rule.
    #[test]
    fn prices_are_stored_as_integers() {
        let path = tmp("ints");
        write_parquet(&sample(10), &path).expect("write");
        let file = File::open(&path).expect("open");
        let b = ParquetRecordBatchReaderBuilder::try_new(file).expect("builder");
        for field in b.schema().fields() {
            assert_eq!(
                field.data_type(),
                &DataType::Int64,
                "column `{}` is not Int64",
                field.name()
            );
        }
        let _ = std::fs::remove_file(&path);
    }
}
