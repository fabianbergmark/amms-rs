use super::util::require;
use crate::errors::AMMError;

pub(crate) fn add_delta(x: u128, y: i128) -> Result<u128, AMMError> {
    if y < 0 {
        let z = x - (-y) as u128;
        require(z < x, "LS")?;
        Ok(z)
    } else {
        let z = x + y as u128;
        require(z >= x, "LA")?;
        Ok(z)
    }
}
