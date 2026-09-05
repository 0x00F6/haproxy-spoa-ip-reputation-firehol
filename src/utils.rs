use chrono::DateTime;

pub fn pretty_number(n: u32) -> String {
    let s = n.to_string();
    let len = s.len();

    s.chars()
        .enumerate()
        .fold(String::with_capacity(len + len / 3), |mut acc, (i, c)| {
            if i > 0 && (len - i).is_multiple_of(3) {
                acc.push(' ');
            }
            acc.push(c);
            acc
        })
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;

    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }

    format!("{:.2} {}", size, UNITS[unit])
}

pub fn epoch_to_string(epoch: i64) -> String {
    DateTime::from_timestamp(epoch, 0)
        .unwrap()
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}
