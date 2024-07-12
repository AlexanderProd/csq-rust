use anyhow::Result;
use std::collections::HashMap;

use serde::{Deserialize, Deserializer, Serialize};

#[derive(Serialize, Debug)]
pub struct CSQExifData {
    #[serde(rename = "OverflowColor")]
    pub overflow_color: Option<String>,
    #[serde(rename = "GPSLongitudeRef")]
    pub gps_longitude_ref: Option<String>,
    #[serde(rename = "GPSImgDirectionRef")]
    pub gps_img_direction_ref: Option<String>,
    #[serde(rename = "FileInodeChangeDate/Time")]
    pub file_inode_change_date_time: Option<String>,
    #[serde(rename = "FilePermissions")]
    pub file_permissions: Option<String>,
    #[serde(rename = "AtmosphericTemperature")]
    pub atmospheric_temperature: f32,
    #[serde(rename = "LensPartNumber")]
    pub lens_part_number: Option<String>,
    #[serde(rename = "CameraTemperatureRangeMin")]
    pub camera_temperature_range_min: Option<String>,
    #[serde(rename = "CameraSoftware")]
    pub camera_software: Option<String>,
    #[serde(rename = "FileName")]
    pub file_name: Option<String>,
    #[serde(rename = "FocusStepCount")]
    pub focus_step_count: Option<String>,
    #[serde(rename = "RawValueRangeMin")]
    pub raw_value_range_min: Option<String>,
    #[serde(rename = "GPSLatitudeRef")]
    pub gps_latitude_ref: Option<String>,
    #[serde(rename = "CreatorSoftware")]
    pub creator_software: Option<String>,
    #[serde(rename = "CameraTemperatureMaxWarn")]
    pub camera_temperature_max_warn: Option<String>,
    #[serde(rename = "RawThermalImageType")]
    pub raw_thermal_image_type: Option<String>,
    #[serde(rename = "Directory")]
    pub directory: Option<String>,
    #[serde(rename = "FileSize")]
    pub file_size: Option<String>,
    #[serde(rename = "CameraTemperatureMinClip")]
    pub camera_temperature_min_clip: Option<String>,
    #[serde(rename = "FilterSerialNumber")]
    pub filter_serial_number: Option<String>,
    #[serde(rename = "ExifToolVersionNumber")]
    pub exif_tool_version_number: Option<String>,
    #[serde(rename = "FilterModel")]
    pub filter_model: Option<String>,
    #[serde(rename = "PlanckO")]
    pub planck_o: f32,
    #[serde(rename = "CameraPartNumber")]
    pub camera_part_number: Option<String>,
    #[serde(rename = "FocusDistance")]
    pub focus_distance: Option<String>,
    #[serde(rename = "BelowColor")]
    pub below_color: Option<String>,
    #[serde(rename = "GPSDilutionOfPrecision")]
    pub gps_dilution_of_precision: Option<String>,
    #[serde(rename = "FileAccessDate/Time")]
    pub file_access_date_time: Option<String>,
    #[serde(rename = "PlanckB")]
    pub planck_b: f32,
    #[serde(rename = "CameraTemperatureMaxSaturated")]
    pub camera_temperature_max_saturated: Option<String>,
    #[serde(rename = "GPSPosition")]
    pub gps_position: Option<String>,
    #[serde(rename = "PaletteMethod")]
    pub palette_method: Option<String>,
    #[serde(rename = "Palette")]
    pub palette: Option<String>,
    #[serde(rename = "Isotherm2Color")]
    pub isotherm2_color: Option<String>,
    #[serde(rename = "IRWindowTransmission")]
    pub ir_window_transmission: f32,
    #[serde(rename = "AtmosphericTransAlpha1")]
    pub atmospheric_trans_alpha1: f32,
    #[serde(rename = "GPSLongitude")]
    pub gps_longitude: Option<String>,
    #[serde(rename = "MIMEType")]
    pub mime_type: Option<String>,
    #[serde(rename = "GPSValid")]
    pub gps_valid: Option<String>,
    #[serde(rename = "GPSAltitude")]
    pub gps_altitude: Option<String>,
    #[serde(rename = "CameraTemperatureRangeMax")]
    pub camera_temperature_range_max: Option<String>,
    #[serde(rename = "FileType")]
    pub file_type: Option<String>,
    #[serde(rename = "RelativeHumidity")]
    pub relative_humidity: f32,
    #[serde(rename = "CameraTemperatureMinWarn")]
    pub camera_temperature_min_warn: Option<String>,
    #[serde(rename = "CameraSerialNumber")]
    pub camera_serial_number: Option<String>,
    #[serde(rename = "CameraTemperatureMaxClip")]
    pub camera_temperature_max_clip: Option<String>,
    #[serde(rename = "UnderflowColor")]
    pub underflow_color: Option<String>,
    #[serde(rename = "RawThermalImage")]
    pub raw_thermal_image: Option<String>,
    #[serde(rename = "GPSLatitude")]
    pub gps_latitude: Option<String>,
    #[serde(rename = "PlanckF")]
    pub planck_f: f32,
    #[serde(rename = "GPSMapDatum")]
    pub gps_map_datum: Option<String>,
    #[serde(rename = "RawValueMedian")]
    pub raw_value_median: Option<String>,
    #[serde(rename = "FileTypeExtension")]
    pub file_type_extension: Option<String>,
    #[serde(rename = "CameraModel")]
    pub camera_model: Option<String>,
    #[serde(rename = "PaletteName")]
    pub palette_name: Option<String>,
    #[serde(rename = "ReflectedApparentTemperature")]
    pub reflected_apparent_temperature: f32,
    #[serde(rename = "Isotherm1Color")]
    pub isotherm1_color: Option<String>,
    #[serde(rename = "AtmosphericTransX")]
    pub atmospheric_trans_x: f32,
    #[serde(rename = "RawValueRange")]
    pub raw_value_range: Option<String>,
    #[serde(rename = "Date/TimeOriginal")]
    pub date_time_original: Option<String>,
    #[serde(rename = "PaletteStretch")]
    pub palette_stretch: Option<String>,
    #[serde(rename = "PlanckR2")]
    pub planck_r2: f32,
    #[serde(rename = "RawThermalImageWidth")]
    pub raw_thermal_image_width: Option<String>,
    #[serde(rename = "PaletteFileName")]
    pub palette_file_name: Option<String>,
    #[serde(rename = "PlanckR1")]
    pub planck_r1: f32,
    #[serde(rename = "FileModificationDate/Time")]
    pub file_modification_date_time: Option<String>,
    #[serde(rename = "LensModel")]
    pub lens_model: Option<String>,
    #[serde(rename = "LensSerialNumber")]
    pub lens_serial_number: Option<String>,
    #[serde(rename = "RawThermalImageHeight")]
    pub raw_thermal_image_height: Option<String>,
    #[serde(rename = "PeakSpectralSensitivity")]
    pub peak_spectral_sensitivity: Option<String>,
    #[serde(rename = "ObjectDistance")]
    pub object_distance: f32,
    #[serde(rename = "AtmosphericTransBeta1")]
    pub atmospheric_trans_beta1: f32,
    #[serde(rename = "IRWindowTemperature")]
    pub ir_window_temperature: f32,
    #[serde(rename = "FieldOfView")]
    pub field_of_view: Option<String>,
    #[serde(rename = "RawValueRangeMax")]
    pub raw_value_range_max: Option<String>,
    #[serde(rename = "FrameRate")]
    pub frame_rate: Option<String>,
    #[serde(rename = "PaletteColors")]
    pub palette_colors: Option<String>,
    #[serde(rename = "GpsImgDirection")]
    pub gps_img_direction: Option<String>,
    #[serde(rename = "Emissivity")]
    pub emissivity: f32,
    #[serde(rename = "AtmosphericTransAlpha2")]
    pub atmospheric_trans_alpha2: f32,
    #[serde(rename = "AtmosphericTransBeta2")]
    pub atmospheric_trans_beta2: f32,
    #[serde(rename = "CameraTemperatureMinSaturated")]
    pub camera_temperature_min_saturated: Option<String>,
    #[serde(rename = "AboveColor")]
    pub above_color: Option<String>,
}

impl<'de> Deserialize<'de> for CSQExifData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let map: HashMap<String, String> = HashMap::deserialize(deserializer)?;

        let get_optional_string = |key: &str| -> Option<String> { map.get(key).cloned() };

        let get_float = |key: &'static str| -> Result<f32, D::Error> {
            map.get(key)
                .ok_or_else(|| serde::de::Error::missing_field(key))?
                .split_whitespace()
                .collect::<Vec<&str>>()
                .first()
                .ok_or_else(|| {
                    serde::de::Error::custom(format!("Failed to split whitespace for {}", key))
                })?
                .parse::<f32>()
                .map_err(|_| serde::de::Error::custom(format!("Failed to parse float for {}", key)))
        };

        Ok(CSQExifData {
            overflow_color: get_optional_string("OverflowColor"),
            gps_longitude_ref: get_optional_string("GPSLongitudeRef"),
            gps_img_direction_ref: get_optional_string("GPSImgDirectionRef"),
            file_inode_change_date_time: get_optional_string("FileInodeChangeDate/Time"),
            file_permissions: get_optional_string("FilePermissions"),
            atmospheric_temperature: get_float("AtmosphericTemperature")?,
            lens_part_number: get_optional_string("LensPartNumber"),
            camera_temperature_range_min: get_optional_string("CameraTemperatureRangeMin"),
            camera_software: get_optional_string("CameraSoftware"),
            file_name: get_optional_string("FileName"),
            focus_step_count: get_optional_string("FocusStepCount"),
            raw_value_range_min: get_optional_string("RawValueRangeMin"),
            gps_latitude_ref: get_optional_string("GPSLatitudeRef"),
            creator_software: get_optional_string("CreatorSoftware"),
            camera_temperature_max_warn: get_optional_string("CameraTemperatureMaxWarn"),
            raw_thermal_image_type: get_optional_string("RawThermalImageType"),
            directory: get_optional_string("Directory"),
            file_size: get_optional_string("FileSize"),
            camera_temperature_min_clip: get_optional_string("CameraTemperatureMinClip"),
            filter_serial_number: get_optional_string("FilterSerialNumber"),
            exif_tool_version_number: get_optional_string("ExifToolVersionNumber"),
            filter_model: get_optional_string("FilterModel"),
            planck_o: get_float("PlanckO")?,
            camera_part_number: get_optional_string("CameraPartNumber"),
            focus_distance: get_optional_string("FocusDistance"),
            below_color: get_optional_string("BelowColor"),
            gps_dilution_of_precision: get_optional_string("GPSDilutionOfPrecision"),
            file_access_date_time: get_optional_string("FileAccessDate/Time"),
            planck_b: get_float("PlanckB")?,
            camera_temperature_max_saturated: get_optional_string("CameraTemperatureMaxSaturated"),
            gps_position: get_optional_string("GPSPosition"),
            palette_method: get_optional_string("PaletteMethod"),
            palette: get_optional_string("Palette"),
            isotherm2_color: get_optional_string("Isotherm2Color"),
            ir_window_transmission: get_float("IRWindowTransmission")?,
            atmospheric_trans_alpha1: get_float("AtmosphericTransAlpha1")?,
            gps_longitude: get_optional_string("GPSLongitude"),
            mime_type: get_optional_string("MIMEType"),
            gps_valid: get_optional_string("GPSValid"),
            gps_altitude: get_optional_string("GPSAltitude"),
            camera_temperature_range_max: get_optional_string("CameraTemperatureRangeMax"),
            file_type: get_optional_string("FileType"),
            relative_humidity: get_float("RelativeHumidity")?,
            camera_temperature_min_warn: get_optional_string("CameraTemperatureMinWarn"),
            camera_serial_number: get_optional_string("CameraSerialNumber"),
            camera_temperature_max_clip: get_optional_string("CameraTemperatureMaxClip"),
            underflow_color: get_optional_string("UnderflowColor"),
            raw_thermal_image: get_optional_string("RawThermalImage"),
            gps_latitude: get_optional_string("GPSLatitude"),
            planck_f: get_float("PlanckF")?,
            gps_map_datum: get_optional_string("GPSMapDatum"),
            raw_value_median: get_optional_string("RawValueMedian"),
            file_type_extension: get_optional_string("FileTypeExtension"),
            camera_model: get_optional_string("CameraModel"),
            palette_name: get_optional_string("PaletteName"),
            reflected_apparent_temperature: get_float("ReflectedApparentTemperature")?,
            isotherm1_color: get_optional_string("Isotherm1Color"),
            atmospheric_trans_x: get_float("AtmosphericTransX")?,
            raw_value_range: get_optional_string("RawValueRange"),
            date_time_original: get_optional_string("Date/TimeOriginal"),
            palette_stretch: get_optional_string("PaletteStretch"),
            planck_r2: get_float("PlanckR2")?,
            raw_thermal_image_width: get_optional_string("RawThermalImageWidth"),
            palette_file_name: get_optional_string("PaletteFileName"),
            planck_r1: get_float("PlanckR1")?,
            file_modification_date_time: get_optional_string("FileModificationDate/Time"),
            lens_model: get_optional_string("LensModel"),
            lens_serial_number: get_optional_string("LensSerialNumber"),
            raw_thermal_image_height: get_optional_string("RawThermalImageHeight"),
            peak_spectral_sensitivity: get_optional_string("PeakSpectralSensitivity"),
            object_distance: get_float("ObjectDistance")?,
            atmospheric_trans_beta1: get_float("AtmosphericTransBeta1")?,
            ir_window_temperature: get_float("IRWindowTemperature")?,
            field_of_view: get_optional_string("FieldOfView"),
            raw_value_range_max: get_optional_string("RawValueRangeMax"),
            frame_rate: get_optional_string("FrameRate"),
            palette_colors: get_optional_string("PaletteColors"),
            gps_img_direction: get_optional_string("GpsImgDirection"),
            emissivity: get_float("Emissivity")?,
            atmospheric_trans_alpha2: get_float("AtmosphericTransAlpha2")?,
            atmospheric_trans_beta2: get_float("AtmosphericTransBeta2")?,
            camera_temperature_min_saturated: get_optional_string("CameraTemperatureMinSaturated"),
            above_color: get_optional_string("AboveColor"),
        })
    }
}
