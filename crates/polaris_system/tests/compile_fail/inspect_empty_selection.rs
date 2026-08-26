//! `inspect()` with an empty list selects nothing, which is always a mistake:
//! bare `inspect` is the spelling for "everything", and the parenthesized form
//! exists to name at least one parameter or `return`.

use polaris_system::system;

#[system(inspect())]
async fn empty_selection() {}

fn main() {}
