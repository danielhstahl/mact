pub fn count(v: &[u8]) -> usize {
    let mut i = 0;
    loop {
        i += 1;
        if i >= 3 {
            break;
        }
    }
    i
}
