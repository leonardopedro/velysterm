use alacritty_terminal::term::cell::Flags;
fn main() {
    println!("{:?}", Flags::BOLD);
    println!("{:?}", Flags::ITALIC);
    println!("{:?}", Flags::INVERSE);
    println!("{:?}", Flags::WRAPLINE);
}
