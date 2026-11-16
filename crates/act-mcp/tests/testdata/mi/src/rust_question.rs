pub fn head(raw: &str) -> Option<u32> {
    let first = raw.split(' ').next()?;
    let Ok(number) = first.parse::<u32>() else {
        return None;
    };
    Some(number + 1)
}
