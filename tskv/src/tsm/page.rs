use std::collections::BTreeMap;
use std::sync::Arc;

use arrow::datatypes::{Field, Schema, SchemaRef};
use datafusion::parquet::data_type::AsBytes;
use models::field_value::FieldVal;
use models::predicate::domain::TimeRange;
use models::schema::{PhysicalCType, TableColumn, TskvTableSchema, TskvTableSchemaRef};
use models::{PhysicalDType, SeriesId, SeriesKey};
use serde::{Deserialize, Serialize};
use utils::bitset::ImmutBitSet;
use utils::BloomFilter;

use super::statistics::ValueStatistics;
use crate::byte_utils::{decode_be_u32, decode_be_u64};
use crate::error::Result;
use crate::tsm::codec::{
    get_bool_codec, get_encoding, get_f64_codec, get_i64_codec, get_str_codec, get_u64_codec,
};
use crate::tsm::writer::Column;
use crate::tsm::ColumnGroupID;
use crate::Error;

#[derive(Debug)]
pub struct Page {
    /// 4 bits for bitset len
    /// 8 bits for data len
    /// 4 bits for crc32 len
    /// bitset len bits for BitSet
    /// the bits of rest for data
    pub(crate) bytes: bytes::Bytes,
    pub(crate) meta: PageMeta,
}

impl Page {
    pub fn new(bytes: bytes::Bytes, meta: PageMeta) -> Self {
        Self { bytes, meta }
    }

    pub fn bytes(&self) -> &bytes::Bytes {
        &self.bytes
    }

    pub fn meta(&self) -> &PageMeta {
        &self.meta
    }

    pub fn desc(&self) -> &TableColumn {
        &self.meta.column
    }

    pub fn crc_validation(&self) -> Result<Page> {
        let bytes = self.bytes().clone();
        let meta = self.meta().clone();
        let data_crc = decode_be_u32(&bytes[12..16]);
        let mut hasher = crc32fast::Hasher::new();
        let bitset_len = decode_be_u32(&self.bytes[0..4]) as usize;
        hasher.update(&bytes[16 + bitset_len..]);
        let data_crc_calculated = hasher.finalize();
        if data_crc != data_crc_calculated {
            // If crc not match, try to return error.
            return Err(Error::TSMPageFileHashCheckFailed {
                crc: data_crc,
                crc_calculated: data_crc_calculated,
                page: Page { bytes, meta },
            });
        }
        Ok(Page { bytes, meta })
    }

    pub fn null_bitset(&self) -> ImmutBitSet<'_> {
        let data_len = decode_be_u64(&self.bytes[4..12]) as usize;
        let bitset_buffer = self.null_bitset_slice();
        ImmutBitSet::new_without_check(data_len, bitset_buffer)
    }

    pub fn null_bitset_slice(&self) -> &[u8] {
        let bitset_len = decode_be_u32(&self.bytes[0..4]) as usize;
        &self.bytes[16..16 + bitset_len]
    }

    pub fn data_buffer(&self) -> &[u8] {
        let bitset_len = decode_be_u32(&self.bytes[0..4]) as usize;
        &self.bytes[16 + bitset_len..]
    }

    pub fn to_column(&self) -> Result<Column> {
        let col_type = self.meta.column.column_type.to_physical_type();
        let mut col = Column::empty_with_cap(col_type.clone(), self.meta.num_values as usize)?;
        let data_buffer = self.data_buffer();
        let bitset = self.null_bitset();
        match col_type {
            PhysicalCType::Tag => {
                return Err(Error::TsmPageError {
                    reason: "tag column not support now".to_string(),
                });
            }
            PhysicalCType::Time(_) | PhysicalCType::Field(PhysicalDType::Integer) => {
                let encoding = get_encoding(data_buffer);
                let ts_codec = get_i64_codec(encoding);
                let mut target = Vec::new();
                ts_codec
                    .decode(data_buffer, &mut target)
                    .map_err(|e| Error::Decode { source: e })?;
                let mut target = target.into_iter();
                for i in 0..bitset.len() {
                    if bitset.get(i) {
                        col.push(Some(FieldVal::Integer(target.next().ok_or(
                            Error::TsmPageError {
                                reason: "data buffer not enough".to_string(),
                            },
                        )?)));
                    } else {
                        col.push(None);
                    }
                }
            }
            PhysicalCType::Field(PhysicalDType::Float) => {
                let encoding = get_encoding(data_buffer);
                let ts_codec = get_f64_codec(encoding);
                let mut target = Vec::new();
                ts_codec
                    .decode(data_buffer, &mut target)
                    .map_err(|e| Error::Decode { source: e })?;
                let mut target = target.into_iter();
                for i in 0..bitset.len() {
                    if bitset.get(i) {
                        col.push(Some(FieldVal::Float(target.next().ok_or(
                            Error::TsmPageError {
                                reason: "data buffer not enough".to_string(),
                            },
                        )?)));
                    } else {
                        col.push(None);
                    }
                }
            }
            PhysicalCType::Field(PhysicalDType::Unsigned) => {
                let encoding = get_encoding(data_buffer);
                let ts_codec = get_u64_codec(encoding);
                let mut target = Vec::new();
                ts_codec
                    .decode(data_buffer, &mut target)
                    .map_err(|e| Error::Decode { source: e })?;
                let mut target = target.into_iter();
                for i in 0..bitset.len() {
                    if bitset.get(i) {
                        col.push(Some(FieldVal::Unsigned(target.next().ok_or(
                            Error::TsmPageError {
                                reason: "data buffer not enough".to_string(),
                            },
                        )?)));
                    } else {
                        col.push(None);
                    }
                }
            }
            PhysicalCType::Field(PhysicalDType::Boolean) => {
                let encoding = get_encoding(data_buffer);
                let ts_codec = get_bool_codec(encoding);
                let mut target = Vec::new();
                ts_codec
                    .decode(data_buffer, &mut target)
                    .map_err(|e| Error::Decode { source: e })?;
                let mut target = target.into_iter();
                for i in 0..bitset.len() {
                    if bitset.get(i) {
                        col.push(Some(FieldVal::Boolean(target.next().ok_or(
                            Error::TsmPageError {
                                reason: "data buffer not enough".to_string(),
                            },
                        )?)));
                    } else {
                        col.push(None);
                    }
                }
            }
            PhysicalCType::Field(PhysicalDType::String) => {
                let encoding = get_encoding(data_buffer);
                let ts_codec = get_str_codec(encoding);
                let mut target = Vec::new();
                ts_codec
                    .decode(data_buffer, &mut target)
                    .map_err(|e| Error::Decode { source: e })?;
                let mut target = target.into_iter();
                for i in 0..bitset.len() {
                    if bitset.get(i) {
                        col.push(Some(FieldVal::Bytes(target.next().ok_or(
                            Error::TsmPageError {
                                reason: "data buffer not enough".to_string(),
                            },
                        )?)));
                    } else {
                        col.push(None);
                    }
                }
            }
            PhysicalCType::Field(PhysicalDType::Unknown) => {
                return Err(Error::UnsupportedDataType {
                    dt: "unknown".to_string(),
                });
            }
        }
        Ok(col)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PageMeta {
    pub(crate) num_values: u32,
    pub(crate) column: TableColumn,
    pub(crate) statistics: PageStatistics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PageStatistics {
    Bool(ValueStatistics<bool>),
    F64(ValueStatistics<f64>),
    I64(ValueStatistics<i64>),
    U64(ValueStatistics<u64>),
    Bytes(ValueStatistics<Vec<u8>>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PageWriteSpec {
    pub(crate) offset: u64,
    pub(crate) size: usize,
    pub(crate) meta: PageMeta,
}

impl PageWriteSpec {
    pub fn new(offset: u64, size: usize, meta: PageMeta) -> Self {
        Self { offset, size, meta }
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// todo: dont copy meta
    pub fn meta(&self) -> &PageMeta {
        &self.meta
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ColumnGroup {
    column_group_id: ColumnGroupID,

    pages_offset: u64,
    size: u64,
    time_range: TimeRange,
    pages: Vec<PageWriteSpec>,
}

impl ColumnGroup {
    pub fn new(id: usize) -> Self {
        Self {
            column_group_id: id,
            pages_offset: 0,
            size: 0,
            time_range: TimeRange::none(),
            pages: Vec::new(),
        }
    }

    pub fn column_group_id(&self) -> ColumnGroupID {
        self.column_group_id
    }

    pub fn time_range_merge(&mut self, time_range: &TimeRange) {
        self.time_range.merge(time_range)
    }

    pub fn pages_offset(&self) -> u64 {
        self.pages_offset
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn time_range(&self) -> &TimeRange {
        &self.time_range
    }

    pub fn pages(&self) -> &[PageWriteSpec] {
        &self.pages
    }

    pub fn push(&mut self, page: PageWriteSpec) {
        if self.pages_offset == 0 {
            self.pages_offset = page.offset;
        }
        if self.size != 0 {
            debug_assert_eq!(self.pages_offset + self.size, page.offset);
        }
        self.size += page.size as u64;
        self.pages.push(page);
    }

    pub fn row_len(&self) -> usize {
        self.pages
            .first()
            .map(|p| p.meta.num_values as usize)
            .unwrap_or(0)
    }

    pub fn time_page_write_spec(&self) -> Result<PageWriteSpec> {
        self.pages
            .iter()
            .find(|p| p.meta.column.column_type.is_time())
            .cloned()
            .ok_or_else(|| Error::TsmColumnGroupError {
                reason: format!("column group: {} not found time page", self.column_group_id),
            })
    }
}

/// A chunk of data for a series at least two columns
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct Chunk {
    time_range: TimeRange,

    table_name: String,
    series_id: SeriesId,
    series_key: SeriesKey,

    next_column_group_id: ColumnGroupID,
    column_groups: BTreeMap<ColumnGroupID, Arc<ColumnGroup>>,
}

impl Chunk {
    pub fn new(table_name: String, series_id: SeriesId, series_key: SeriesKey) -> Self {
        Self {
            time_range: TimeRange::none(),
            table_name,
            series_id,
            series_key,
            next_column_group_id: 0,
            column_groups: Default::default(),
        }
    }

    pub fn min_ts(&self) -> i64 {
        self.time_range.min_ts
    }

    pub fn max_ts(&self) -> i64 {
        self.time_range.max_ts
    }

    pub fn len(&self) -> usize {
        self.column_groups.len()
    }

    pub fn column_group(&self) -> &BTreeMap<ColumnGroupID, Arc<ColumnGroup>> {
        &self.column_groups
    }

    pub fn next_column_group_id(&mut self) -> ColumnGroupID {
        let id = self.next_column_group_id;
        self.next_column_group_id += 1;
        id
    }

    pub fn current_next_column_group_id(&self) -> ColumnGroupID {
        self.next_column_group_id
    }

    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    pub fn series_id(&self) -> SeriesId {
        self.series_id
    }

    pub fn series_key(&self) -> &SeriesKey {
        &self.series_key
    }
    pub fn serialize(&self) -> Result<Vec<u8>> {
        bincode::serialize(&self).map_err(|e| Error::Serialize { source: e.into() })
    }

    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        bincode::deserialize(bytes).map_err(|e| Error::Deserialize { source: e.into() })
    }

    pub fn push(&mut self, column_group: Arc<ColumnGroup>) -> Result<()> {
        if self.time_range.max_ts > column_group.time_range.min_ts {
            return Err(Error::TsmColumnGroupError {
                reason: format!(
                    "invalid column group time range, current max_ts: {}, new min_ts: {}",
                    self.time_range.max_ts, column_group.time_range.min_ts
                ),
            });
        }
        self.time_range.merge(&column_group.time_range);
        if self
            .column_groups
            .get(&column_group.column_group_id())
            .is_some()
        {
            return Err(Error::TsmColumnGroupError {
                reason: format!(
                    "duplicate column group id: {}, failed push pages meta to tsm_meta",
                    column_group.column_group_id()
                ),
            });
        }
        self.column_groups
            .insert(column_group.column_group_id(), column_group);
        Ok(())
    }
    pub fn time_range(&self) -> &TimeRange {
        &self.time_range
    }
    /// TODO high performance cost
    pub fn schema(&self) -> SchemaRef {
        if let Some((_, cg)) = self.column_group().first_key_value() {
            let fields = cg
                .pages()
                .iter()
                .map(|p| (&p.meta().column).into())
                .collect::<Vec<Field>>();
            return Arc::new(Schema::new(fields));
        }

        Arc::new(Schema::empty())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChunkWriteSpec {
    pub(crate) series_id: SeriesId,
    pub(crate) chunk_offset: u64,
    pub(crate) chunk_size: usize,
    pub(crate) statics: ChunkStatics,
}

impl ChunkWriteSpec {
    pub fn new(
        series_id: SeriesId,
        chunk_offset: u64,
        chunk_size: usize,
        statics: ChunkStatics,
    ) -> Self {
        Self {
            series_id,
            chunk_offset,
            chunk_size,
            statics,
        }
    }

    pub fn series_id(&self) -> SeriesId {
        self.series_id
    }

    pub fn chunk_offset(&self) -> u64 {
        self.chunk_offset
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    pub fn statics(&self) -> &ChunkStatics {
        &self.statics
    }
}

/// ChunkStatics
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChunkStatics {
    pub(crate) time_range: TimeRange,
}

/// A group of chunks for a table
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct ChunkGroup {
    pub(crate) chunks: Vec<ChunkWriteSpec>,
}

impl ChunkGroup {
    pub fn new() -> Self {
        Self { chunks: Vec::new() }
    }
    pub fn serialize(&self) -> Result<Vec<u8>> {
        bincode::serialize(&self).map_err(|e| Error::Serialize { source: e.into() })
    }

    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        bincode::deserialize(bytes).map_err(|e| Error::Deserialize { source: e.into() })
    }

    pub fn push(&mut self, chunk: ChunkWriteSpec) {
        self.chunks.push(chunk);
    }

    pub fn len(&self) -> usize {
        self.chunks.len()
    }
    pub fn time_range(&self) -> TimeRange {
        let mut time_range = TimeRange::none();
        for chunk in self.chunks.iter() {
            time_range.merge(&chunk.statics.time_range);
        }
        time_range
    }

    pub fn chunks(&self) -> &[ChunkWriteSpec] {
        &self.chunks
    }
}

pub type TableId = u64;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChunkGroupWriteSpec {
    // pub(crate) id: TableId,
    pub(crate) table_schema: Arc<TskvTableSchema>,
    pub(crate) chunk_group_offset: u64,
    pub(crate) chunk_group_size: usize,
    pub(crate) time_range: TimeRange,
    pub(crate) count: usize,
}

impl ChunkGroupWriteSpec {
    pub fn new(
        table_schema: TskvTableSchemaRef,
        chunk_group_offset: u64,
        chunk_group_size: usize,
        time_range: TimeRange,
        count: usize,
    ) -> Self {
        Self {
            table_schema,
            chunk_group_offset,
            chunk_group_size,
            time_range,
            count,
        }
    }

    pub fn name(&self) -> &str {
        &self.table_schema.name
    }

    pub fn chunk_group_offset(&self) -> u64 {
        self.chunk_group_offset
    }

    pub fn chunk_group_size(&self) -> usize {
        self.chunk_group_size
    }

    pub fn time_range(&self) -> &TimeRange {
        &self.time_range
    }

    pub fn count(&self) -> usize {
        self.count
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChunkGroupMeta {
    // table name -> chunk group meta
    tables: BTreeMap<String, ChunkGroupWriteSpec>,
}

impl Default for ChunkGroupMeta {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkGroupMeta {
    pub fn new() -> Self {
        Self {
            tables: BTreeMap::new(),
        }
    }
    pub fn serialize(&self) -> Result<Vec<u8>> {
        bincode::serialize(&self).map_err(|e| Error::Serialize { source: e.into() })
    }

    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        bincode::deserialize(bytes).map_err(|e| Error::Deserialize { source: e.into() })
    }

    pub fn push(&mut self, table: ChunkGroupWriteSpec) {
        self.tables.insert(table.table_schema.name.clone(), table);
    }
    pub fn len(&self) -> usize {
        self.tables.len()
    }
    pub fn time_range(&self) -> TimeRange {
        let mut time_range = TimeRange::none();
        for (_, table) in self.tables.iter() {
            time_range.merge(&table.time_range);
        }
        time_range
    }

    pub fn tables(&self) -> &BTreeMap<String, ChunkGroupWriteSpec> {
        &self.tables
    }

    pub fn table_schema(&self, table_name: &str) -> Option<Arc<TskvTableSchema>> {
        self.tables.get(table_name).map(|t| t.table_schema.clone())
    }
}

// pub const FOOTER_SIZE: i64 = ;

#[derive(Debug, Default, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct Footer {
    pub(crate) version: u8,
    pub(crate) time_range: TimeRange,
    pub(crate) table: TableMeta,
    pub(crate) series: SeriesMeta,
}

impl Footer {
    pub fn new(version: u8, time_range: TimeRange, table: TableMeta, series: SeriesMeta) -> Self {
        Self {
            version,
            time_range,
            table,
            series,
        }
    }

    pub fn table(&self) -> &TableMeta {
        &self.table
    }

    pub fn series(&self) -> &SeriesMeta {
        &self.series
    }

    pub fn time_range(&self) -> &TimeRange {
        &self.time_range
    }

    pub fn version(&self) -> u8 {
        self.version
    }

    pub fn serialize(&self) -> Result<Vec<u8>> {
        bincode::serialize(&self).map_err(|e| Error::Serialize { source: e.into() })
    }

    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        bincode::deserialize(bytes).map_err(|e| Error::Deserialize { source: e.into() })
    }

    pub fn maybe_series_exist(&self, series_id: &SeriesId) -> bool {
        self.series
            .bloom_filter
            .maybe_contains((*series_id).as_bytes().as_ref())
    }
}

///  7 + 8 + 8 = 23
#[derive(Clone, Debug, Default, Serialize, Deserialize, Eq, PartialEq)]
pub struct TableMeta {
    // todo: bloomfilter, store table object id
    // bloom_filter: BloomFilter,
    chunk_group_offset: u64,
    chunk_group_size: usize,
}

impl TableMeta {
    pub fn new(chunk_group_offset: u64, chunk_group_size: usize) -> Self {
        Self {
            chunk_group_offset,
            chunk_group_size,
        }
    }

    pub fn chunk_group_offset(&self) -> u64 {
        self.chunk_group_offset
    }

    pub fn chunk_group_size(&self) -> usize {
        self.chunk_group_size
    }
}

/// 16 + 8 + 8 = 32
#[derive(Clone, Debug, Default, Serialize, Deserialize, Eq, PartialEq)]
pub struct SeriesMeta {
    bloom_filter: BloomFilter,
    // 16 Byte
    chunk_offset: u64,
    chunk_size: u64,
}

impl SeriesMeta {
    pub fn new(bloom_filter: Vec<u8>, chunk_offset: u64, chunk_size: u64) -> Self {
        let bloom_filter = BloomFilter::with_data(&bloom_filter);
        Self {
            bloom_filter,
            chunk_offset,
            chunk_size,
        }
    }

    pub fn serialize(&self) -> Result<Vec<u8>> {
        bincode::serialize(&self).map_err(|e| Error::Serialize { source: e.into() })
    }

    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        bincode::deserialize(bytes).map_err(|e| Error::Deserialize { source: e.into() })
    }

    pub fn bloom_filter(&self) -> &BloomFilter {
        &self.bloom_filter
    }

    pub fn chunk_offset(&self) -> u64 {
        self.chunk_offset
    }

    pub fn chunk_size(&self) -> u64 {
        self.chunk_size
    }
}

#[cfg(test)]
mod test {
    use arrow::datatypes::ToByteSlice;
    use models::predicate::domain::TimeRange;
    use models::schema::{ColumnType, TableColumn};
    use models::ValueType;
    use utils::bitset::BitSet;
    use utils::BloomFilter;

    use crate::tsm::page::{Footer, Page, PageMeta, PageStatistics, SeriesMeta, TableMeta};
    use crate::tsm::statistics::ValueStatistics;
    use crate::tsm::BLOOM_FILTER_BITS;

    fn create_test_page() -> Page {
        let field_column = TableColumn::new(
            1,
            "field1".to_string(),
            ColumnType::Field(ValueType::Integer),
            Default::default(),
        );

        let pagemeta = PageMeta {
            num_values: 1,
            column: field_column,
            statistics: PageStatistics::I64(ValueStatistics::new(Some(1), Some(3), None, 1)),
        };

        let buf = b"hello world".to_byte_slice();
        let data_len = 1_u64;
        let valid = BitSet::new();
        let len_bitset = valid.byte_len() as u32;

        let mut hasher = crc32fast::Hasher::new();
        hasher.update(buf);
        let data_crc = hasher.finalize().to_be_bytes();

        let mut data = vec![];
        data.extend_from_slice(&len_bitset.to_be_bytes());
        data.extend_from_slice(&data_len.to_be_bytes());
        data.extend_from_slice(&data_crc);
        data.extend_from_slice(valid.bytes());
        data.extend_from_slice(buf);

        let bytes = bytes::Bytes::from(data);
        Page::new(bytes, pagemeta.clone())
    }

    #[test]
    fn test1() {
        let table_meta = TableMeta {
            chunk_group_offset: 100,
            chunk_group_size: 100,
        };
        let expect_footer = Footer::new(
            1,
            TimeRange {
                min_ts: 0,
                max_ts: 100,
            },
            table_meta,
            SeriesMeta::new(
                BloomFilter::new(BLOOM_FILTER_BITS).bytes().to_vec(),
                100,
                100,
            ),
        );
        let bytess = expect_footer.serialize().unwrap();
        println!("bytes: {:?}", bytess.len());
        let footer = Footer::deserialize(&bytess).unwrap();
        assert_eq!(footer, expect_footer);
    }

    #[test]
    fn test_page_crc_validation() {
        let page = create_test_page();
        let result = page.crc_validation();
        assert!(result.is_ok());
    }
}
