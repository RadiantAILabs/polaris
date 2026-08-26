//! A `where` clause on a system function must be rejected: the generated
//! struct and factory carry no generics, so the clause would be silently
//! discarded.

use polaris_system::system;

#[system]
async fn where_clause_system()
where
    i32: Copy,
{
}

fn main() {}
