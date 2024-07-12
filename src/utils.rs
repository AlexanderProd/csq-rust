use std::env;

use anyhow::{anyhow, Result};
use ndarray::{Array2, ShapeBuilder};
use pyo3::prelude::*;

use crate::types::CSQExifData;

// The images in the CSQ file are old style JPEGs.
// https://github.com/haraldk/TwelveMonkeys/issues/67
pub fn decode_jpeg_py(img: &[u8]) -> Result<Array2<f32>> {
    let decoded = Python::with_gil(|py| -> PyResult<Vec<Vec<f32>>> {
        #[cfg(target_os = "macos")]
        if let Ok(venv) = env::var("VIRTUAL_ENV") {
            let sys = py.import_bound("sys")?;
            let syspath = sys.getattr("path")?;
            let version_info = py.version_info();

            syspath.call_method1(
                "append",
                (format!(
                    "{}/lib/python{}.{}/site-packages",
                    venv, version_info.major, version_info.minor
                ),),
            )?;
        }

        let libjpeg = PyModule::import_bound(py, "pylibjpeg")
            .expect("Failed to import libjpeg python module");

        let res = libjpeg.getattr("decode")?.call1((img,))?;

        let v = res.extract::<Vec<Vec<f32>>>()?;

        Ok(v)
    })?;

    let cols = decoded.len();
    let first_col_length = &decoded[0].len();
    let rows = if cols > 0 { *first_col_length } else { 0 };

    let flat: Vec<f32> = decoded.into_iter().flatten().collect();

    let arr = Array2::from_shape_vec((rows, cols).f(), flat.to_vec())
        .map_err(|e| anyhow!("Failed to create ndarray: {e}"))?
        .reversed_axes();

    Ok(arr)
}

pub fn raw_to_temp(raw: &Array2<f32>, metadata: CSQExifData) -> Result<Box<Array2<f32>>> {
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

    let raw_refl1 = pr1 / (pr2 * ((pb / (r_temp + 273.15)).exp() - pf)) - po;
    let raw_refl1_attn = (1.0 - e) / e * raw_refl1;

    let raw_atm1 = pr1 / (pr2 * ((pb / (a_temp + 273.15)).exp() - pf)) - po;
    let raw_atm1_attn = (1.0 - tau1) / e / tau1 * raw_atm1;

    let raw_wind = pr1 / (pr2 * ((pb / (ir_w_temp + 273.15)).exp() - pf)) - po;
    let raw_wind_attn = emiss_wind / e / tau1 / irt * raw_wind;

    let raw_refl2 = pr1 / (pr2 * ((pb / (r_temp + 273.15)).exp() - pf)) - po;
    let raw_refl2_attn = refl_wind / e / tau1 / irt * raw_refl2;

    let raw_atm2 = pr1 / (pr2 * ((pb / (a_temp + 273.15)).exp() - pf)) - po;
    let raw_atm2_attn = (1.0 - tau2) / e / tau1 / irt / tau2 * raw_atm2;

    let raw_obj = raw / e / tau1 / irt / tau2
        - raw_atm1_attn
        - raw_atm2_attn
        - raw_wind_attn
        - raw_refl1_attn
        - raw_refl2_attn;

    let temp_c = pb / (pr1 / (pr2 * (&raw_obj + po)) + pf).mapv(|x| x.ln()) - 273.15;

    let temp_box = Box::new(temp_c);

    Ok(temp_box)
}
