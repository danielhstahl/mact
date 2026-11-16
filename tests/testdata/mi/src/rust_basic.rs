use std::io;

pub struct Gate {
    limit: u32,
}

impl Gate {
    pub fn check(&self, value: u32, label: &str) -> io::Result<bool> {
        if value == 0 || label.is_empty() {
            return Ok(false);
        }
        let rest = match self.limit.checked_sub(value) {
            Some(rest) if rest > 2 => rest,
            Some(rest) => rest,
            None => return Ok(true),
        };
        let mut total = 0;
        for i in 0..3 {
            total += i;
        }
        while total > rest {
            total -= 1;
        }
        let joined = vec!["a", "b"].join(",");
        Ok(total > rest && joined.len() > 1)
    }
}
