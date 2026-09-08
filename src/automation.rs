use std::{marker::PhantomData, ops::Deref, rc::Rc};

use anyhow::{Context as _, Result};
use uiautomation::UIAutomation;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

// UIAutomation::new initializes COM on every call without balancing it on
// drop. Own that initialization explicitly, including S_FALSE on WGC threads
// that already initialized WinRT. Neither the client nor its guard may move
// to another thread.
pub(crate) struct AutomationClient {
    client: UIAutomation,
    _apartment: Apartment,
}

struct Apartment(PhantomData<Rc<()>>);

impl Apartment {
    fn new() -> Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .context("UI Automation用のCOMを初期化できませんでした")?;
        Ok(Self(PhantomData))
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

impl AutomationClient {
    pub(crate) fn new() -> Result<Self> {
        let apartment = Apartment::new()?;
        let client =
            UIAutomation::new_direct().context("Windows UI Automationを初期化できませんでした")?;
        Ok(Self {
            client,
            _apartment: apartment,
        })
    }
}

impl Deref for AutomationClient {
    type Target = UIAutomation;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use windows::Win32::System::Com::{
        APTTYPE, APTTYPEQUALIFIER, APTTYPEQUALIFIER_IMPLICIT_MTA, CoGetApartmentType,
    };

    fn com_initialized() -> bool {
        let mut kind = APTTYPE::default();
        let mut qualifier = APTTYPEQUALIFIER::default();
        unsafe { CoGetApartmentType(&mut kind, &mut qualifier) }.is_ok()
            && qualifier != APTTYPEQUALIFIER_IMPLICIT_MTA
    }

    #[test]
    fn repeated_uia_clients_balance_com_initialization() {
        std::thread::spawn(|| {
            assert!(!com_initialized());
            for _ in 0..3 {
                let client = AutomationClient::new().unwrap();
                assert!(com_initialized());
                drop(client);
                assert!(!com_initialized());
            }
        })
        .join()
        .unwrap();
    }

    #[test]
    fn uia_client_preserves_callers_existing_apartment() {
        std::thread::spawn(|| {
            let apartment = Apartment::new().unwrap();
            drop(AutomationClient::new().unwrap());
            assert!(com_initialized());
            drop(apartment);
            assert!(!com_initialized());
        })
        .join()
        .unwrap();
    }
}
