// The same tokens as rust_reformat_b.rs, in different bytes.
fn sum(v: &[u8]) -> u32 {
    let mut t = 0;
    for b in v { t += *b as u32; }
    if t > 8 { t = 8; }
    t
}
