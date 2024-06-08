module a::m {

    public struct A(u64) has copy, drop;

    public struct B has copy, drop {
        a: A
    }

    public fun t0(_s: A): u64 { abort 0 }
    public fun t1(_s: B): u64 { abort 0 }
}

module a::n {
    use a::m::{A,B};

    fun foo(_a: A, _b: B) {
        let _tmp1 = _a.;
        let _tmp2 = _b.;
    }
}
