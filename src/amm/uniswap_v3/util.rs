use crate::errors::AMMError;

pub(crate) fn require(assertion: bool, message: &'static str) -> Result<(), AMMError> {
    if assertion {
        Ok(())
    } else {
        Err(AMMError::LogicError(message))
    }
}
