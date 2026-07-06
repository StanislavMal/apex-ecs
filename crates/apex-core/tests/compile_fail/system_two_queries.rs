//! F6: every query parameter accumulates into one `Self::Query`, so a second
//! query would silently AND-join rather than be independent. `system!` rejects
//! it and tells the user to combine into a single tuple query.

apex_core::system! {
    fn two_queries(q1: (Foo,), q2: (Bar,)) {}
}

fn main() {}
