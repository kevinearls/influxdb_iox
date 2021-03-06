use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use entry::{test_helpers::lp_to_entries, Entry};
use flate2::read::GzDecoder;
use mutable_buffer::chunk::{Chunk, ChunkMetrics};
use std::io::Read;

#[inline]
fn write_chunk(count: usize, entries: &[Entry]) {
    // m0 is hard coded into tag_values.lp.gz
    let mut chunk = Chunk::new("m0", ChunkMetrics::new_unregistered());

    for _ in 0..count {
        for entry in entries {
            for write in entry.partition_writes().iter().flatten() {
                for batch in write.table_batches() {
                    chunk.write_table_batch(1, 5, batch).unwrap();
                }
            }
        }
    }
}

fn load_entries() -> Vec<Entry> {
    let raw = include_bytes!("../../tests/fixtures/lineproto/tag_values.lp.gz");
    let mut gz = GzDecoder::new(&raw[..]);
    let mut lp = String::new();
    gz.read_to_string(&mut lp).unwrap();
    lp_to_entries(&lp)
}

pub fn write_mb(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_mb");
    let entries = load_entries();
    for count in &[1, 2, 3, 4, 5] {
        group.bench_function(BenchmarkId::from_parameter(count), |b| {
            b.iter(|| write_chunk(*count, &entries));
        });
    }
    group.finish();
}

criterion_group!(benches, write_mb);
criterion_main!(benches);
