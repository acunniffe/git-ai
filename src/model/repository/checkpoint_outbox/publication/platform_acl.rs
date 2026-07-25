use crate::model::repository::checkpoint_outbox::CheckpointOutboxError;
use std::fs::File;

#[cfg(target_os = "linux")]
pub(super) fn reject_unsafe(_file: &File) -> Result<(), CheckpointOutboxError> {
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn clear_inherited(_file: &File) -> Result<(), CheckpointOutboxError> {
    Ok(())
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::ptr;

    type Acl = *mut libc::c_void;
    type AclEntry = *mut libc::c_void;

    const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;
    const ACL_FIRST_ENTRY: libc::c_int = 0;
    const ACL_NEXT_ENTRY: libc::c_int = -1;
    const ACL_EXTENDED_ALLOW: libc::c_int = 1;
    const ACL_EXTENDED_DENY: libc::c_int = 2;

    unsafe extern "C" {
        fn acl_free(object: *mut libc::c_void) -> libc::c_int;
        fn acl_get_entry(acl: Acl, entry_id: libc::c_int, entry: *mut AclEntry) -> libc::c_int;
        fn acl_get_fd_np(fd: libc::c_int, acl_type: libc::c_int) -> Acl;
        fn acl_get_tag_type(entry: AclEntry, tag_type: *mut libc::c_int) -> libc::c_int;
        fn acl_init(count: libc::c_int) -> Acl;
        fn acl_set_fd_np(fd: libc::c_int, acl: Acl, acl_type: libc::c_int) -> libc::c_int;
    }

    struct OwnedAcl(Acl);

    impl OwnedAcl {
        fn read(file: &File) -> io::Result<Self> {
            let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
            if acl.is_null() {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::NotFound {
                    Self::empty()
                } else {
                    Err(error)
                }
            } else {
                Ok(Self(acl))
            }
        }

        fn empty() -> io::Result<Self> {
            let acl = unsafe { acl_init(0) };
            if acl.is_null() {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self(acl))
            }
        }

        fn has_entries(&self) -> io::Result<bool> {
            self.visit_tags(|_| true)
        }

        fn has_unsafe_allow(&self) -> io::Result<bool> {
            self.visit_tags(|tag| tag != ACL_EXTENDED_DENY)
        }

        fn visit_tags(&self, reject: impl Fn(libc::c_int) -> bool) -> io::Result<bool> {
            let mut entry_id = ACL_FIRST_ENTRY;
            loop {
                let mut entry = ptr::null_mut();
                clear_errno();
                let result = unsafe { acl_get_entry(self.0, entry_id, &mut entry) };
                if result != 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::EINVAL) {
                        return Ok(false);
                    }
                    return Err(error);
                }

                let mut tag = 0;
                if unsafe { acl_get_tag_type(entry, &mut tag) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                if tag == ACL_EXTENDED_ALLOW || reject(tag) {
                    return Ok(true);
                }
                entry_id = ACL_NEXT_ENTRY;
            }
        }
    }

    impl Drop for OwnedAcl {
        fn drop(&mut self) {
            unsafe {
                acl_free(self.0);
            }
        }
    }

    pub(super) fn reject_unsafe(file: &File) -> Result<(), CheckpointOutboxError> {
        let acl =
            OwnedAcl::read(file).map_err(|error| acl_io_error("inspect extended ACL", error))?;
        if acl
            .has_unsafe_allow()
            .map_err(|error| acl_io_error("inspect extended ACL", error))?
        {
            return Err(CheckpointOutboxError::UnsafeReadyRecord);
        }
        Ok(())
    }

    pub(super) fn clear_inherited(file: &File) -> Result<(), CheckpointOutboxError> {
        let empty = OwnedAcl::empty().map_err(|error| acl_io_error("allocate empty ACL", error))?;
        if unsafe { acl_set_fd_np(file.as_raw_fd(), empty.0, ACL_TYPE_EXTENDED) } != 0 {
            return Err(acl_io_error(
                "clear inherited ACL",
                io::Error::last_os_error(),
            ));
        }
        file.sync_all()
            .map_err(|error| acl_io_error("sync cleared ACL", error))?;

        let current =
            OwnedAcl::read(file).map_err(|error| acl_io_error("verify cleared ACL", error))?;
        if current
            .has_entries()
            .map_err(|error| acl_io_error("verify cleared ACL", error))?
        {
            return Err(CheckpointOutboxError::UnsafeReadyRecord);
        }
        Ok(())
    }

    fn clear_errno() {
        unsafe {
            *libc::__error() = 0;
        }
    }

    fn acl_io_error(operation: &'static str, error: io::Error) -> CheckpointOutboxError {
        CheckpointOutboxError::Io {
            operation,
            kind: error.kind(),
        }
    }
}

#[cfg(target_os = "macos")]
pub(super) fn reject_unsafe(file: &File) -> Result<(), CheckpointOutboxError> {
    macos::reject_unsafe(file)
}

#[cfg(target_os = "macos")]
pub(super) fn clear_inherited(file: &File) -> Result<(), CheckpointOutboxError> {
    macos::clear_inherited(file)
}
