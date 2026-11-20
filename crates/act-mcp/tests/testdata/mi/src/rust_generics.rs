#![allow(dead_code)]

/// Doc for `T: Clone`.
pub struct Pair<T: Clone>(T, T);

impl<'a, T: Clone + 'a> Pair<T> {
    #[inline]
    pub fn pick<V>(&self, key: &&str, flag: bool) -> Option<&'a V>
    where
        V: Default,
    {
        let c = 'x';
        let raw = r#"not ( a token"#;
        let both = *key;
        if flag && true || key.is_empty() {
            return None;
        }
        match self.0.clone().cmp(&self.1) {
            std::cmp::Ordering::Less => Some(&self.0),
            _ => None,
        }
    }
}
