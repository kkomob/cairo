#[derive(Copy, Drop, Serde)]
enum MemberType {
    Simple: felt252,
    Complex: Span<Member>,
}

#[derive(Copy, Drop, Serde)]
struct Member {
    name: felt252,
    ty: MemberType,
}

#[test]
#[available_gas(100000)]
fn sierra_type_dup() {
    let mut arr = array![];
    MemberType::Simple('ty').serialize(ref arr);
}
