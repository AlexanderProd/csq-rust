use anyhow::Result;
use ndarray::Array2;

use crate::types::CSQExifData;

const CELCIUS_OFFSET: f32 = 273.15;

pub fn vec_u8_to_f32(vec: &[u8]) -> Vec<f32> {
    assert!(vec.len() % 2 == 0, "The length of the vector must be even");

    vec.chunks(2)
        .map(|chunk| {
            // Combine two bytes into a u16 integer
            let u16 = (chunk[0] as u16) | ((chunk[1] as u16) << 8);

            // Convert to f32 since we're going to calculate with f32's later anyways
            u16 as f32
        })
        .collect()
}

pub fn raw_to_temp(
    metadata: &CSQExifData,
    radiance_values: &Array2<f32>,
) -> Result<Box<Array2<f32>>> {
    let e = metadata.emissivity;
    let od = metadata.object_distance;
    let r_temp = metadata.reflected_apparent_temperature;
    let a_temp = metadata.atmospheric_temperature;
    let ir_w_temp = metadata.ir_window_temperature;
    let irt = metadata.ir_window_transmission;
    let rh = metadata.relative_humidity;
    let pr1 = metadata.planck_r1;
    let pb = metadata.planck_b;
    let pf = metadata.planck_f;
    let po = metadata.planck_o;
    let pr2 = metadata.planck_r2;
    let ata1 = metadata.atmospheric_trans_alpha1;
    let ata2 = metadata.atmospheric_trans_alpha2;
    let atb1 = metadata.atmospheric_trans_beta1;
    let atb2 = metadata.atmospheric_trans_beta2;
    let atx = metadata.atmospheric_trans_x;

    let emiss_wind = 1.0 - irt;
    let refl_wind = 0.0;

    let h2o = (rh / 100.0)
        * ((1.5587 + 0.06939 * a_temp - 0.00027816 * a_temp.powi(2)
            + 0.00000068455 * a_temp.powi(3))
        .exp());

    let tau1 = atx * (-(od / 2.0).sqrt() * (ata1 + atb1 * h2o.sqrt())).exp()
        + (1.0 - atx) * (-(od / 2.0).sqrt() * (ata2 + atb2 * h2o.sqrt())).exp();

    let tau2 = atx * (-(od / 2.0).sqrt() * (ata1 + atb1 * h2o.sqrt())).exp()
        + (1.0 - atx) * (-(od / 2.0).sqrt() * (ata2 + atb2 * h2o.sqrt())).exp();
    // Note: for this script, we assume the thermal window is at the mid-point (OD/2) between the source and the camera sensor

    let raw_refl1 = pr1 / (pr2 * ((pb / (r_temp + CELCIUS_OFFSET)).exp() - pf)) - po;
    let raw_refl1_attn = (1.0 - e) / e * raw_refl1;

    let raw_atm1 = pr1 / (pr2 * ((pb / (a_temp + CELCIUS_OFFSET)).exp() - pf)) - po;
    let raw_atm1_attn = (1.0 - tau1) / e / tau1 * raw_atm1;

    let raw_wind = pr1 / (pr2 * ((pb / (ir_w_temp + CELCIUS_OFFSET)).exp() - pf)) - po;
    let raw_wind_attn = emiss_wind / e / tau1 / irt * raw_wind;

    let raw_refl2 = pr1 / (pr2 * ((pb / (r_temp + CELCIUS_OFFSET)).exp() - pf)) - po;
    let raw_refl2_attn = refl_wind / e / tau1 / irt * raw_refl2;

    let raw_atm2 = pr1 / (pr2 * ((pb / (a_temp + CELCIUS_OFFSET)).exp() - pf)) - po;
    let raw_atm2_attn = (1.0 - tau2) / e / tau1 / irt / tau2 * raw_atm2;

    let raw_obj = radiance_values / e / tau1 / irt / tau2
        - raw_atm1_attn
        - raw_atm2_attn
        - raw_wind_attn
        - raw_refl1_attn
        - raw_refl2_attn;

    let temp_c = pb / (pr1 / (pr2 * (&raw_obj + po)) + pf).mapv(|x| x.ln()) - CELCIUS_OFFSET;

    let temp_box = Box::new(temp_c);

    Ok(temp_box)
}
