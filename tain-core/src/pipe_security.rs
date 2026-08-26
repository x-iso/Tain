//! Who may open Tain's named pipes, and the handle hygiene around them.
//!
//! # Why this is its own module
//!
//! There are two pipes — one per desktop for frames, one to the service for
//! configuration — and they need exactly the same answer to exactly the same
//! question. Written twice, the second copy is where the access list quietly
//! drifts from the first; and of everything in this application, the access
//! list on these pipes is the last thing that should be maintained in two
//! places.
//!
//! # The answer
//!
//! Local System, and the user signed in at the console. Nobody else.
//!
//! One pipe carries pictures of the secure desktop and the clicks that answer
//! it; the other carries the host's identity. A default security descriptor
//! would let any authenticated user open either, which on a machine with more
//! than one account means one user watching and driving another's consent
//! prompts — a boundary Windows spends real effort maintaining, and not one to
//! give away by leaving a parameter null.
//!
//! `D:P` makes the list protected, so nothing is inherited: what is written
//! here is the whole of it rather than a starting point somebody's group policy
//! can add to.

use anyhow::Result;

#[cfg(target_os = "windows")]
pub use platform::{console_user_sid, wide, OwnedHandle, PipeSecurity};

#[cfg(not(target_os = "windows"))]
pub fn console_user_sid() -> Result<String> {
    anyhow::bail!("named pipe security is Windows-only")
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use anyhow::Context;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION_1,
        },
        PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    };

    /// Null-terminated UTF-16, which is what every call here wants.
    pub fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Closes a handle however the surrounding code exits.
    pub struct OwnedHandle(pub HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                // SAFETY: owned by this type, closed once.
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }

    /// The SID of the user signed in at the console, as a string.
    ///
    /// A caller running as Local System has to ask which user that is. One
    /// running as the user already *is* the answer, so it reads its own token —
    /// `WTSQueryUserToken` would refuse it there, and the result would be the
    /// same anyway.
    pub fn console_user_sid() -> Result<String> {
        use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
        use windows::Win32::System::RemoteDesktop::{
            WTSGetActiveConsoleSessionId, WTSQueryUserToken,
        };
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        // SAFETY: a documented read of global session state.
        let session = unsafe { WTSGetActiveConsoleSessionId() };

        // SAFETY: both paths produce a token this function then owns.
        let token = unsafe {
            let mut token = HANDLE::default();
            if WTSQueryUserToken(session, &mut token).is_ok() {
                OwnedHandle(token)
            } else {
                let mut own = HANDLE::default();
                OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut own)
                    .context("could not open this process's token")?;
                OwnedHandle(own)
            }
        };

        // SAFETY: a two-call query into a buffer sized by the first call.
        unsafe {
            let mut needed = 0u32;
            let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut needed);
            let mut buffer = vec![0u8; needed as usize];
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                needed,
                &mut needed,
            )
            .context("could not read the user out of the token")?;

            let user = &*(buffer.as_ptr() as *const TOKEN_USER);
            let mut text = windows::core::PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &mut text)
                .context("could not render the user's SID")?;
            let owned = text.to_string().context("the SID was not valid text")?;
            let _ = LocalFree(HLOCAL(text.0 as *mut _));
            Ok(owned)
        }
    }

    /// A security descriptor and the attributes pointing at it.
    ///
    /// Kept together because the attributes borrow the descriptor: freeing it
    /// while a pipe creation is still reading through them is the kind of
    /// use-after-free that works nine times out of ten.
    pub struct PipeSecurity {
        descriptor: PSECURITY_DESCRIPTOR,
        attributes: SECURITY_ATTRIBUTES,
    }

    impl PipeSecurity {
        /// Local System and the console user, and nobody else.
        pub fn for_console_user(user_sid: &str) -> Result<Self> {
            let sddl = wide(&format!("D:P(A;;GA;;;SY)(A;;GRGW;;;{user_sid})"));
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            // SAFETY: the string outlives the call; the descriptor is freed on
            // drop.
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    PCWSTR(sddl.as_ptr()),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    None,
                )
            }
            .context("could not build the pipe's access list")?;

            let attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: false.into(),
            };
            Ok(Self { descriptor, attributes })
        }

        /// For handing to `CreateNamedPipeW`. Valid while `self` is.
        pub fn attributes(&self) -> &SECURITY_ATTRIBUTES {
            &self.attributes
        }
    }

    impl Drop for PipeSecurity {
        fn drop(&mut self) {
            if !self.descriptor.is_invalid() {
                // SAFETY: allocated by the conversion above, freed once.
                unsafe {
                    let _ = LocalFree(HLOCAL(self.descriptor.0));
                }
            }
        }
    }
}
