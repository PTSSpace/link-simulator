/// Link Simulator
///
/// Copyright (C) 2019 PTScientists GmbH
///
/// 17.04.2019    Eric Reinthal

use std::fmt;

/// custom library result and boxing
pub type LinkResult<T> = std::result::Result<T, Box<std::error::Error>>;

/// custom error type for whole library
#[derive(Debug)]
pub struct LinkError {
    message: String,
}

impl LinkError {
    fn new(message: String) -> Self {
        LinkError { message }
    }
}

pub fn err_msg<S: Into<String>>(message: S) -> Box<LinkError> {
    LinkError::new(message.into()).into()
}

/// shortcut for creating an instance of the LinkError Err trait
///
/// argument can be a single String or same input as for 'format!' macro
macro_rules! link_error {
    ($arg:ty) => {
        Err(err_msg(($arg)))
    };
    ($($arg:tt)*) => {
        Err(err_msg(format!($($arg)*)))
    };
}

impl std::error::Error for LinkError {}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}
