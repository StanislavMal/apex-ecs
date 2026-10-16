//! ADR-022: a stateless `system!` is a function, so a binding spelled like a system compiles.
//! Its own test target: if the macro fails to parse, rustc never reaches name resolution, and a
//! witness sharing a file with `pub`/attribute systems could not show this class on its own.

use apex_core::system;
use apex_macros::Component;

#[derive(Component)]
struct Counter(u32);

// The field report (HEAD 213f303): a stateless system was a unit struct named like the function,
// and a unit struct captures every binding of that name in its module — `|_, (counter,)|` next to
// a system `counter` read as the struct pattern (E0530, E0308). A function name captures nothing:
// this target passes by compiling.

system! {
    fn counter(q: (&mut Counter,)) {
        q.for_each_mut(|_, (mut c,)| {
            c.0 += 1;
        });
    }
}

system! {
    fn reads_counter(q: (&Counter,)) {
        q.for_each(|_, (counter,)| {
            let _ = counter.0;
        });
    }
}

#[test]
fn a_let_and_a_closure_bind_the_name_of_a_system() {
    let counter = 3_u32;
    let twice = |counter: u32| counter * 2;
    assert_eq!(twice(counter), 6);
}
