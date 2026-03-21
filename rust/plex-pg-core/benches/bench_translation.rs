use criterion::{black_box, criterion_group, criterion_main, Criterion};
use plex_pg_core::translate;

fn bench_translation(c: &mut Criterion) {
    let queries = [
        "SELECT * FROM metadata_items WHERE id = ?",
        "SELECT id, title, rating FROM metadata_items WHERE parent_id = ? ORDER BY \"index\"",
        "INSERT INTO metadata_items (title, added_at) VALUES (?, ?)",
        "UPDATE metadata_items SET updated_at = ? WHERE id = ?",
        "SELECT * FROM media_parts WHERE media_item_id = ? COLLATE NOCASE",
        "SELECT id, title FROM metadata_items WHERE title LIKE ? COLLATE NOCASE",
        "SELECT * FROM metadata_items WHERE added_at > strftime('%s', 'now', '-7 days')",
        "SELECT iif(rating > 5, 'good', 'bad') FROM metadata_items WHERE id = ?",
        "SELECT IFNULL(title, 'Unknown') FROM metadata_items",
        "SELECT * FROM metadata_items WHERE id IN (?, ?, ?) ORDER BY RANDOM() LIMIT 10",
    ];

    for (idx, sql) in queries.iter().enumerate() {
        let name = format!("translate/query_{}", idx + 1);
        c.bench_function(&name, |b| {
            b.iter(|| {
                let t = translate(black_box(sql)).unwrap();
                black_box(t.sql);
            })
        });
    }
}

criterion_group!(benches, bench_translation);
criterion_main!(benches);
