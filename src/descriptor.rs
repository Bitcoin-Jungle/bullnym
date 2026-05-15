use crate::error::AppError;
use std::str::FromStr;

pub fn derive_address(ct_descriptor: &str, index: u32) -> Result<String, AppError> {
    let desc: lwk_wollet::WolletDescriptor = ct_descriptor
        .parse()
        .map_err(|e| AppError::InvalidDescriptor(format!("{e}")))?;

    let addr = desc
        .address(index, &lwk_wollet::elements::AddressParams::LIQUID)
        .map_err(|e| AppError::InvalidDescriptor(format!("address derivation failed: {e}")))?;

    Ok(addr.to_string())
}

pub fn derive_blinding_key_hex(
    ct_descriptor: &str,
    liquid_address: &str,
) -> Result<String, AppError> {
    let desc: elements_miniscript::ConfidentialDescriptor<
        elements_miniscript::DescriptorPublicKey,
    > = ct_descriptor
        .parse()
        .map_err(|e| AppError::InvalidDescriptor(format!("{e}")))?;
    let addr = lwk_wollet::elements::Address::from_str(liquid_address)
        .map_err(|e| AppError::InvalidDescriptor(format!("liquid address parse failed: {e}")))?;
    let key = lwk_common::derive_blinding_key(&desc, &addr.script_pubkey()).ok_or_else(|| {
        AppError::InvalidDescriptor("descriptor cannot derive blinding key".into())
    })?;
    Ok(key.display_secret().to_string())
}

pub fn validate_descriptor(ct_descriptor: &str, max_len: usize) -> Result<(), AppError> {
    if ct_descriptor.len() > max_len {
        return Err(AppError::InvalidDescriptor(format!(
            "descriptor exceeds maximum length of {max_len}"
        )));
    }
    derive_address(ct_descriptor, 0)?;
    Ok(())
}

#[cfg(test)]
mod tests;
