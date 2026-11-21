use crate::UserRow;
use crate::parse::key_append;
use arrow_array::builder::StringBuilder;
use arrow_array::{ArrayRef, Date32Array, Int32Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{Field, Schema};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use std::path::Path;
use std::sync::Arc;

fn write(path: &Path, cols: Vec<(&str, ArrayRef, bool)>) {
    let schema = Arc::new(Schema::new(
        cols.iter().map(|(n, a, null)| Field::new(*n, a.data_type().clone(), *null)).collect::<Vec<_>>(),
    ));
    let batch = RecordBatch::try_new(schema.clone(), cols.into_iter().map(|c| c.1).collect()).unwrap();
    let props = WriterProperties::builder()
        .set_compression(Compression::UNCOMPRESSED)
        .set_dictionary_enabled(false)
        .set_statistics_enabled(EnabledStatistics::None)
        .build();
    let file = std::fs::File::create(path).unwrap();
    let mut w = ArrowWriter::try_new(file, schema, Some(props)).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn s(v: &[u8]) -> &str {
    std::str::from_utf8(v).unwrap_or_else(|_| unsafe { std::str::from_utf8_unchecked(v) })
}

pub fn write_all(
    out: &Path,
    rows: &[Vec<UserRow>],
    cats: &[Vec<u8>],
    camps: &[(Vec<u8>, Vec<u8>)],
    camp_rows: &[(usize, i32, u64, u64, u64, u64)],
    threads: usize,
) {
    let ud = out.join("user_daily");
    let cd = out.join("campaign_daily");
    std::fs::create_dir_all(&ud).unwrap();
    std::fs::create_dir_all(&cd).unwrap();
    const PART: usize = 1 << 18;
    let mut parts: Vec<&[UserRow]> = rows.iter().flat_map(|r| r.chunks(PART)).collect();
    if parts.is_empty() {
        parts.push(&[]);
    }
    std::thread::scope(|sc| {
        sc.spawn(|| {
            crate::par_tasks(parts.len(), threads, || (), |_, i| {
                let part = parts[i];
                let mut user = StringBuilder::with_capacity(part.len(), part.len() * 10);
                let mut name = Vec::with_capacity(16);
                for r in part {
                    name.clear();
                    key_append(r.pkey, &mut name);
                    user.append_value(s(&name));
                }
                let user = user.finish();
                let top = StringArray::from_iter(
                    part.iter().map(|r| (r.top != u32::MAX).then(|| s(&cats[r.top as usize]))),
                );
                write(
                    &ud.join(format!("part-{i:05}.parquet")),
                    vec![
                        ("user_id", Arc::new(user), false),
                        ("day", Arc::new(Date32Array::from_iter_values(part.iter().map(|r| r.day))), false),
                        ("sessions", Arc::new(Int32Array::from_iter_values(part.iter().map(|r| r.sessions as i32))), false),
                        ("events", Arc::new(Int32Array::from_iter_values(part.iter().map(|r| r.events as i32))), false),
                        ("revenue_cents", Arc::new(Int64Array::from_iter_values(part.iter().map(|r| r.revenue as i64))), false),
                        ("top_category", Arc::new(top), true),
                    ],
                );
            });
        });
        sc.spawn(|| {
            let r = camp_rows;
            write(
                &cd.join("part-00000.parquet"),
                vec![
                    ("campaign_id", Arc::new(StringArray::from_iter_values(r.iter().map(|x| s(&camps[x.0].0)))), false),
                    ("channel", Arc::new(StringArray::from_iter_values(r.iter().map(|x| s(&camps[x.0].1)))), false),
                    ("day", Arc::new(Date32Array::from_iter_values(r.iter().map(|x| x.1))), false),
                    ("sessions", Arc::new(Int64Array::from_iter_values(r.iter().map(|x| x.2 as i64))), false),
                    ("purchasing_sessions", Arc::new(Int64Array::from_iter_values(r.iter().map(|x| x.3 as i64))), false),
                    ("revenue_cents", Arc::new(Int64Array::from_iter_values(r.iter().map(|x| x.4 as i64))), false),
                    ("assisted_sessions", Arc::new(Int64Array::from_iter_values(r.iter().map(|x| x.5 as i64))), false),
                ],
            );
        });
    });
}
