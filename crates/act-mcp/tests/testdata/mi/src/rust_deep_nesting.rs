fn grade(scores: &[u32], flag: bool) -> u32 {
    let mut total = 0;
    for score in scores {
        if *score > 90 {
            for tweak in 0..2 {
                if tweak > 0 && flag {
                    total += tweak;
                } else if tweak < 1 || !flag {
                    total -= 1;
                }
            }
        } else if *score > 60 || flag {
            if total > 10 && flag {
                total += 2;
            } else {
                total -= 1;
            }
        } else {
            match score {
                0 => total -= 1,
                _ => total += 1,
            }
        }
    }
    total
}
