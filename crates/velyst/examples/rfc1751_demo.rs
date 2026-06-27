use velyst::rfc1751::u64_to_rfc1751;

fn main() {
    let numbers = vec![0, 1, 1024, 2047, 2048, 1234567890, u64::MAX];

    for &n in &numbers {
        println!("{:20} -> {}", n, u64_to_rfc1751(n));
    }
}
