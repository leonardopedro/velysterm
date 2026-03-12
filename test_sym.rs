fn main() {
    let expr = quantrs2_symengine_pure::parser::parse("c_0 * a_0 + 0.5 * (C_0 * A_0)^2").unwrap();
    println!("{}", expr);
}
