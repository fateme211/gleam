import one.{A, A as C, B, B as D}

/// For these statements we use the records in a qualified fashion
pub const qualified_const_a = one.A

pub fn qualified_fn_a() {
  qualified_const_a
}

pub const qualified_const_b = one.B(one.A, one.A)

pub fn qualified_fn_b() {
  qualified_const_b
}

/// For these statements we use the records in a unqualified fashion
pub const unqualified_const_a = A

pub fn unqualified_fn_a() {
  unqualified_const_a
}

pub const unqualified_const_b = B(A, A)

pub fn unqualified_fn_b() {
  unqualified_const_b
}

/// For these statements we use the records in a unqualified and also aliased
/// fashion
pub const aliased_const_a = C

pub fn aliased_fn_a() {
  aliased_const_a
}

pub const aliased_const_b = D(C, C)

pub fn aliased_fn_b() {
  aliased_const_b
}

/// For these statements we use the accessors for the record from the other
/// module
pub fn accessors(user: one.User) {
  let name = user.name
  let score = user.score
  #(name, score)
}
