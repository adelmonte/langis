//! `cargo run -p langis-emoji --example query -- smi thu "heart eyes"`
fn main() {
    let opts = langis_emoji::SearchOptions::default();
    for q in std::env::args().skip(1) {
        let start = std::time::Instant::now();
        let results = langis_emoji::search(&q, &opts);
        let elapsed = start.elapsed();
        println!("{q:?} ({elapsed:?})");
        for m in results {
            println!("  {:>6.1}  {}  :{}:  {}", m.score, m.emoji.glyph, m.emoji.shortcode().unwrap_or("-"), m.emoji.name);
        }
    }
}
