pub fn normalize_pair(a: u8, b: u8) -> (u8, u8) {
    fn rule_30(a: u8) -> u8 {
        a ^ ((a << 1) | (a << 2))
    }

    let carry = a & 1;
    let mut a = a >> 1;
    let mut b = (carry << 7) | b;
    a = rule_30(a);
    b = rule_30(b);
    let msb = b >> 7;
    a = (a << 1) | msb;

    (a, b)
}
