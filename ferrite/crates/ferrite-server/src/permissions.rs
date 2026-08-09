use crate::api::{ApiError, AuthedUser};
use ferrite_core::RemoteError;

/// Checks whether `user` may access `path` on `remote`, requiring write access
/// when `write` is true. Admins always pass; standard users need a covering
/// `PathGrant` (see `AuthedUser::can_access`).
pub fn authorize(user: &AuthedUser, remote: &str, path: &str, write: bool) -> Result<(), ApiError> {
    if user.can_access(remote, path, write) {
        Ok(())
    } else {
        Err(ApiError(RemoteError::Forbidden(format!(
            "You do not have {} access to '{}'",
            if write { "write" } else { "read" },
            path
        ))))
    }
}
