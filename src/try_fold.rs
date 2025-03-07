use crate::valid::Valid;

/// Trait for types that support a "try fold" operation.
///
/// `TryFolding` describes a composable folding operation that can potentially fail.
/// It can optionally consume an input to transform the provided value.
pub struct TryFold<'a, I: 'a, O: 'a, E: 'a>(Box<dyn Fn(&I, O) -> Valid<O, E> + 'a>);

impl<'a, I, O: Clone + 'a, E> TryFold<'a, I, O, E> {
  /// Try to fold the value with the input.
  ///
  /// # Parameters
  /// - `input`: The input used in the folding operation.
  /// - `value`: The value to be folded.
  ///
  /// # Returns
  /// Returns a `Valid` value, which can be either a success with the folded value
  /// or an error.
  pub fn try_fold(&self, input: &I, state: O) -> Valid<O, E> {
    (self.0)(input, state)
  }

  /// Combine two `TryFolding` implementors into a sequential operation.
  ///
  /// This method allows for chaining two `TryFolding` operations, where the result of the first operation
  /// (if successful) will be used as the input for the second operation.
  ///
  /// # Parameters
  /// - `other`: Another `TryFolding` implementor.
  ///
  /// # Returns
  /// Returns a combined `And` structure that represents the sequential folding operation.
  pub fn and(self, other: TryFold<'a, I, O, E>) -> Self {
    TryFold(Box::new(move |input, state| {
      self
        .try_fold(input, state.clone())
        .fold(|value| other.try_fold(input, value), other.try_fold(input, state))
    }))
  }

  /// Create a new `TryFold` with a specified folding function.
  ///
  /// # Parameters
  /// - `f`: The folding function.
  ///
  /// # Returns
  /// Returns a new `TryFold` instance.
  pub fn new(f: impl Fn(&I, O) -> Valid<O, E> + 'a) -> Self {
    TryFold(Box::new(f))
  }

  /// Tries to fold all items in the provided iterator.
  ///
  /// # Parameters
  /// - `items`: A list of items implementing `TryFolding`.
  ///
  /// # Returns
  /// Returns a `Collect` instance that can be used to perform a folding operation
  /// over all the items in the list.
  pub fn from_iter<F: IntoIterator<Item = TryFold<'a, I, O, E>>>(items: F) -> TryFold<'a, I, O, E> {
    let mut iter = items.into_iter();
    let head = iter.next();

    if let Some(head) = head {
      head.and(TryFold::from_iter(iter))
    } else {
      TryFold::empty()
    }
  }

  pub fn transform<O1>(self, up: impl Fn(O) -> O1 + 'a, down: impl Fn(O1) -> O + 'a) -> TryFold<'a, I, O1, E> {
    self.transform_valid(move |o| Valid::succeed(up(o)), move |o1| Valid::succeed(down(o1)))
  }

  pub fn transform_valid<O1>(
    self,
    up: impl Fn(O) -> Valid<O1, E> + 'a,
    down: impl Fn(O1) -> Valid<O, E> + 'a,
  ) -> TryFold<'a, I, O1, E> {
    TryFold(Box::new(move |input, o1| {
      down(o1).and_then(|o| self.try_fold(input, o)).and_then(|o| up(o))
    }))
  }

  /// Create a `TryFold` that always succeeds with the provided state.
  ///
  /// # Parameters
  /// - `state`: The state to succeed with.
  ///
  /// # Returns
  /// Returns a `TryFold` that always succeeds with the provided state.
  pub fn succeed(state: O) -> Self {
    TryFold(Box::new(move |_, _| Valid::succeed(state.clone())))
  }

  /// Create a `TryFold` that doesn't do anything.
  ///
  /// # Returns
  /// Returns a `TryFold` that doesn't do anything.
  pub fn empty() -> Self {
    TryFold::new(|_, o| Valid::succeed(o))
  }
}

#[cfg(test)]
mod tests {
  use super::TryFold;
  use crate::valid::{Valid, ValidationError};

  #[test]
  fn test_and() {
    let t1 = TryFold::<i32, i32, ()>::new(|a: &i32, b: i32| Valid::succeed(a + b));
    let t2 = TryFold::<i32, i32, ()>::new(|a: &i32, b: i32| Valid::succeed(a * b));
    let t = t1.and(t2);

    let actual = t.try_fold(&2, 3).to_result().unwrap();
    let expected = 10;

    assert_eq!(actual, expected)
  }

  #[test]
  fn test_combine_ok() {
    let t1 = TryFold::<i32, i32, ()>::new(|a: &i32, b: i32| Valid::succeed(a + b));
    let t2 = TryFold::<i32, i32, ()>::new(|a: &i32, b: i32| Valid::succeed(a * b));
    let t = t1.and(t2);

    let actual = t.try_fold(&2, 3).to_result().unwrap();
    let expected = 10;

    assert_eq!(actual, expected)
  }

  #[test]
  fn test_one_failure() {
    let t1 = TryFold::new(|a: &i32, b: i32| Valid::fail(a + b));
    let t2 = TryFold::new(|a: &i32, b: i32| Valid::succeed(a * b));
    let t = t1.and(t2);

    let actual = t.try_fold(&2, 3).to_result().unwrap_err();
    let expected = ValidationError::new(5);

    assert_eq!(actual, expected)
  }

  #[test]
  fn test_both_failure() {
    let t1 = TryFold::new(|a: &i32, b: i32| Valid::fail(a + b));
    let t2 = TryFold::new(|a: &i32, b: i32| Valid::fail(a * b));
    let t = t1.and(t2);

    let actual = t.try_fold(&2, 3).to_result().unwrap_err();
    let expected = ValidationError::new(5).combine(ValidationError::new(6));

    assert_eq!(actual, expected)
  }

  #[test]
  fn test_1_3_failure_left() {
    let t1 = TryFold::new(|a: &i32, b: i32| Valid::fail(a + b)); // 2 + 3
    let t2 = TryFold::new(|a: &i32, b: i32| Valid::succeed(a * b)); // 2 * 3
    let t3 = TryFold::new(|a: &i32, b: i32| Valid::fail(a * b * 100)); // 2 * 6
    let t = t1.and(t2).and(t3);

    let actual = t.try_fold(&2, 3).to_result().unwrap_err();
    let expected = ValidationError::new(5).combine(ValidationError::new(600));

    assert_eq!(actual, expected)
  }

  #[test]
  fn test_1_3_failure_right() {
    let t1 = TryFold::new(|a: &i32, b: i32| Valid::fail(a + b)); // 2 + 3
    let t2 = TryFold::new(|a: &i32, b: i32| Valid::succeed(a * b)); // 2 * 3
    let t3 = TryFold::new(|a: &i32, b: i32| Valid::fail(a * b * 100)); // 2 * 6
    let t = t1.and(t2.and(t3));

    let actual = t.try_fold(&2, 3).to_result().unwrap_err();
    let expected = ValidationError::new(5).combine(ValidationError::new(1200));

    assert_eq!(actual, expected)
  }

  #[test]
  fn test_2_3_failure() {
    let t1 = TryFold::new(|a: &i32, b: i32| Valid::succeed(a + b));
    let t2 = TryFold::new(|a: &i32, b: i32| Valid::fail(a * b));
    let t3 = TryFold::new(|a: &i32, b: i32| Valid::fail(a * b * 100));
    let t = t1.and(t2.and(t3));

    let actual = t.try_fold(&2, 3).to_result().unwrap_err();
    let expected = ValidationError::new(10).combine(ValidationError::new(1000));

    assert_eq!(actual, expected)
  }

  #[test]
  fn test_try_all() {
    let t1 = TryFold::new(|a: &i32, b: i32| Valid::succeed(a + b));
    let t2 = TryFold::new(|a: &i32, b: i32| Valid::fail(a * b));
    let t3 = TryFold::new(|a: &i32, b: i32| Valid::fail(a * b * 100));
    let t = TryFold::from_iter(vec![t1, t2, t3]);

    let actual = t.try_fold(&2, 3).to_result().unwrap_err();
    let expected = ValidationError::new(10).combine(ValidationError::new(1000));

    assert_eq!(actual, expected)
  }

  #[test]
  fn test_try_all_1_3_fail() {
    let t1 = TryFold::new(|a: &i32, b: i32| Valid::fail(a + b));
    let t2 = TryFold::new(|a: &i32, b: i32| Valid::succeed(a * b));
    let t3 = TryFold::new(|a: &i32, b: i32| Valid::fail(a * b * 100));
    let t = TryFold::from_iter(vec![t1, t2, t3]);

    let actual = t.try_fold(&2, 3).to_result().unwrap_err();
    let expected = ValidationError::new(5).combine(ValidationError::new(1200));

    assert_eq!(actual, expected)
  }
}

#[test]
fn test_transform() {
  let t: TryFold<'_, i32, String, ()> = TryFold::new(|a: &i32, b: i32| Valid::succeed(a + b))
    .transform(|v: i32| v.to_string(), |v: String| v.parse::<i32>().unwrap());

  let actual = t.try_fold(&2, "3".to_string()).to_result().unwrap();
  let expected = "5".to_string();

  assert_eq!(actual, expected)
}
