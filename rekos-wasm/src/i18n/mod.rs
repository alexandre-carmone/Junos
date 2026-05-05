//! Internationalization — UI strings loaded from per-language JSON files.
//!
//! `en.json` and `fr.json` (sibling files) hold the actual strings. They are
//! embedded at compile time via `include_str!`, parsed once on first access,
//! and cached as a `&'static Translations`. Adding a new field means: declare
//! it in the `translations!` invocation below, then add the entry to *both*
//! JSON files. A missing key panics the first time the language is requested.

use std::collections::HashMap;
use std::sync::OnceLock;

// ── Language enum ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub enum Lang {
    #[default]
    En,
    Fr,
}

impl Lang {
    pub fn label(self) -> &'static str {
        match self {
            Self::En => "EN",
            Self::Fr => "FR",
        }
    }

    pub fn toggle(self) -> Self {
        match self {
            Self::En => Self::Fr,
            Self::Fr => Self::En,
        }
    }
}

// ── Translations: schema + builder, declared once via macro ─────────────────

macro_rules! translations {
    ($($name:ident),* $(,)?) => {
        pub struct Translations {
            $(pub $name: &'static str,)*
        }

        fn build(map: &serde_json::Map<String, serde_json::Value>) -> Translations {
            Translations {
                $($name: leak(map, stringify!($name)),)*
            }
        }
    };
}

translations! {
    connect, disconnect, start, stop, status, disconnected, tab_sky, tab_camera,
    tab_align, tab_devices, tab_indiserver, tab_log, tab_sky_abbr, tab_focus_abbr, tab_imaging_abbr, tab_polar_abbr,
    tab_guide_abbr, tab_scheduler_abbr, tab_mosaic_abbr, tab_mosaic, tab_profiles_abbr, tab_profiles, profiles_title, profiles_add,
    profiles_edit, profiles_delete, profiles_save, profiles_cancel, profiles_launch, profiles_stop, profiles_active, profiles_mode,
    profiles_mode_local, profiles_mode_remote, profiles_host, profiles_port, profiles_auto_connect, profiles_port_selector, profiles_guiding, profiles_web_manager,
    profiles_remote_drivers, profiles_drivers, profiles_name, profiles_confirm_delete, profiles_confirm_launch, profiles_starting, profiles_empty, profiles_new,
    no_devices, role_grid_title, all_devices_title, auto_connect_label, none_option, no_mount_assigned, no_camera_assigned, telescope_title,
    focal_length_mm, aperture_mm, guide_scope_title, guide_focal_length_mm, guide_aperture_mm, connection, connected_label, connect_existing,
    start_local, running, stopped, no_drivers, search_drivers, selected_label, save_profile, load_profile,
    profile_name, delete_label, profiles, profile_modified, no_log_entries, toggle_controls, sky_section, objects_section,
    settings_section, stars_checkbox, names_checkbox, constellations, grid, eq_grid, fov, all_dso,
    galaxies, open_clusters, globular_clusters, nebulae, planetary_nebulae, supernova_remnants, galaxy_clusters, mag_limit,
    fl_mm, follow_mount, location_section, latitude_label, longitude_label, set_location_btn, get_location_btn, goto_btn,
    now, reset, goto_here, cancel, search_placeholder, ecliptic, zenith, solar_system,
    solve_marker, slew_trail, cursor, info_close, body_sun, body_moon, body_mercury, body_venus,
    body_mars, body_jupiter, body_saturn, body_uranus, body_neptune, mag_label, size_label, phase_label,
    type_label, kind_star, kind_galaxy, kind_open_cluster, kind_globular, kind_nebula, kind_planetary, kind_snr,
    kind_galaxy_cluster, kind_planet, kind_moon, kind_sun, goto_and_align, goto_align_btn, align_btn, align_iteration,
    align_error, align_target, align_solved, align_abort, align_accuracy, align_exposure, align_idle, align_defaults_title,
    align_max_iterations, save_defaults_btn, tab_polar_align, pa_direction, pa_east, pa_west, pa_rotation, pa_exposure,
    pa_start, pa_abort, pa_refresh, pa_step, pa_az_error, pa_alt_error, pa_total, pa_idle,
    no_camera, exposing, exposing_prefix, s_remaining, idle, duration_s_label, gain, offset,
    bin, expose_btn, abort_btn, parameters, frame, cooling, temp, cooler_on,
    cooler_off, target, set_btn, preview, sequence, path, object, no_sequence_items,
    type_col, progress, dur_s, add_btn, frame_light, frame_dark, frame_bias, frame_flat,
    seq_start, seq_pause, seq_resume, seq_abort, seq_clear, seq_flat_panel_on, seq_flat_intensity, seq_close_dust_cap,
    seq_target_adu, solver_settings, index_dir_label, scale_low_label, scale_high_label, timeout_label, downsample_label, solve_radius_label,
    save_btn, mount_label, camera_label, host_label, port_label, indiserver_label, tab_mount, mount_park,
    mount_unpark, mount_abort, mount_sync_to_pole, mount_slew_rate, mount_motion_hold, mount_hour_angle, mount_time_to_meridian, mount_pier_side,
    mount_time_to_flip, mount_flip_due, mount_auto_flip, mount_flip_delay, mount_flip_autofocus, mount_past_meridian, meridian, tab_focus,
    focus_start, focus_abort, focus_measure_hfr, focus_position, focus_hfr, focus_stars, focus_step_size, focus_num_steps,
    focus_algorithm, focus_star_detect, focus_curve_fit, focus_tolerance, focus_idle, focus_log, focus_curve, focus_move_to,
    focus_step_in, focus_step_out, focus_focuser_label, focus_max_iter, focus_backlash, focus_sep_profile, tab_flat_cal, fc_light_panel,
    fc_panel_device, fc_light_on, fc_light_off, fc_intensity, fc_adu_optimizer, fc_target_adu, fc_tolerance_pct, fc_initial_exp,
    fc_max_iterations, fc_min_exp, fc_max_exp, fc_start, fc_abort, fc_state, fc_measured_adu, fc_current_exp,
    fc_optimal_exp, fc_idle, fc_preview, fc_histogram, fc_log, fc_no_log, tab_dust_cap, dc_device,
    dc_park, dc_unpark, tab_filter_wheel, fw_device, fw_current_filter, fw_move_to, fw_config_title, fw_filter_name,
    fw_target_adu, fw_flat_exposure, fw_panel_intensity, fw_focus_offset, fw_save_config, tab_guide, tab_scheduler, hide_tabs,
    show_tabs, tab_imaging, tab_files, tab_mount_abbr, files_title, files_no_selection, files_raw_header, files_breadcrumb_root,
    files_capture_basics, files_optical, files_astrometry, files_filename, files_size, files_mtime, files_exposure, files_gain,
    files_binning, files_frame_type, files_filter, files_target, files_focal, files_pixel_size, files_temp, files_ra,
    files_dec, files_fov, files_rotation, files_plate_solved, files_parent, files_loading, files_error, files_empty_dir,
    livestack_title, livestack_init, livestack_close, livestack_start, livestack_stop, livestack_settings, livestack_dir_in, livestack_dir_out,
    livestack_state, livestack_frames, livestack_snr, livestack_align_method, livestack_stack_method, livestack_low_sigma, livestack_high_sigma, livestack_looping,
    livestack_calc_snr, livestack_apply, livestack_latest, livestack_no_state, mount_title, mount_coords_section, mount_ra_jnow, mount_dec_jnow,
    mount_ra_j2000, mount_dec_j2000, mount_az, mount_alt, mount_ha, mount_pier_west, mount_pier_east, mount_pier_unknown,
    mount_status_idle, mount_status_slewing, mount_status_tracking, mount_status_parking, mount_status_parked, mount_status_error, mount_goto_section, mount_ra_input,
    mount_dec_input, mount_target_input, mount_j2000_label, mount_goto_btn, mount_goto_target_btn, mount_sync_btn, mount_park_btn, mount_unpark_btn,
    mount_abort_btn, mount_tracking_on, mount_tracking_off, mount_meridian_flip, mount_autopark, mount_no_device, cardinal_n, cardinal_e,
    cardinal_s, cardinal_w, zenith_mark, solved_mark, overlay_lst, overlay_fov, overlay_center, overlay_alt,
    overlay_az, overlay_mount, overlay_mount_none, overlay_camera_angle, overlay_time_offset, overlay_cursor, ra_label, dec_label,
    jnow_label, j2000_label, yes, no, focus_actions_section, focus_manual_section, focus_settings_section, focus_header_focuser,
    focus_header_hfr, focus_header_position, focus_header_temperature, focus_capture_btn, focus_loop_btn, focus_reset_frame, focus_step_label, focus_in_btn,
    focus_out_btn, focus_no_frame, focus_settings_not_loaded, imaging_camera, imaging_temp, imaging_cooler, imaging_sensor, imaging_progress,
    imaging_cooler_on_val, imaging_cooler_off_val, imaging_toggle_preview_title, imaging_hide_preview, imaging_show_preview, imaging_no_frame, imaging_capture_controls, imaging_collapse_all,
    imaging_expand_all, imaging_actions, imaging_cooling, imaging_exposure, imaging_frame, imaging_gain_iso, imaging_filter, imaging_target,
    imaging_job_temperature, imaging_target_c, imaging_set, imaging_sequence_queue, imaging_add_job, imaging_empty_queue, imaging_remove_job, field_exposure_s,
    field_frame_type, field_count, field_delay_s, field_bin_x, field_bin_y, field_format, field_encoding, field_gain,
    field_offset, field_iso, field_filter, field_target_name, field_directory, field_enforce_temp, field_job_temp_c, pa_enabled_label,
    pa_pre_start, pa_rotation_deg_label, pa_mount_speed_label, pa_manual_slew_label, pa_start_btn_long, pa_stop_btn_long, pa_capture_solve, pa_running,
    pa_abort_short, pa_manual_rotation_section, pa_manual_rotate_instr, pa_rotation_done, pa_refresh_correct, pa_exposure_s_label, pa_algorithm_label, pa_original,
    pa_updated, pa_total_label, pa_az_label_long, pa_alt_label_long, pa_start_refresh, pa_stop_refresh, pa_algo_plate_solve, pa_algo_move_star,
    pa_algo_move_star_calc, pa_speed_max, guide_guider_label, guide_rms, guide_connected, guide_actions, guide_essentials, guide_ra_dec_corrections,
    guide_calibration, guide_dither, guide_algorithms, guide_backend, guide_phd2, guide_linguider, guide_advanced, guide_gpg,
    guide_start, guide_capture, guide_loop, guide_clear_cal, guide_capture_loop_note, guide_dither_note, guide_internal, guide_phd2_label,
    guide_linguider_label, guide_host, guide_port, guide_f_exposure, guide_f_delay, guide_f_gain, guide_f_binning, guide_f_tracking_box,
    guide_f_dark_frame, guide_f_subframe, guide_f_auto_star, guide_f_stream, guide_f_ra_guiding, guide_f_east_pulses, guide_f_west_pulses, guide_f_dec_guiding,
    guide_f_north_pulses, guide_f_south_pulses, guide_f_iterations, guide_f_pulse_duration, guide_f_max_move, guide_f_two_axis, guide_f_auto_box_size, guide_f_dec_backlash,
    guide_f_reset_each_start, guide_f_reuse_cal, guide_f_reverse_dec_flip, guide_f_dither_enabled, guide_f_dither_amount, guide_f_dither_frames, guide_f_dither_settle_thr, guide_f_dither_settle_t,
    guide_f_dither_timeout, guide_f_dither_max_iter, guide_f_dither_one_pulse, guide_f_dither_fail_abort, guide_f_dither_no_guiding, guide_f_dither_no_guide_pulse, guide_f_detection, guide_f_ra_pulse_algo,
    guide_f_dec_pulse_algo, guide_f_ra_kp, guide_f_ra_ki, guide_f_ra_min_pulse, guide_f_ra_max_pulse, guide_f_ra_hysteresis, guide_f_dec_kp, guide_f_dec_ki,
    guide_f_dec_min_pulse, guide_f_dec_max_pulse, guide_f_dec_hysteresis, guide_f_max_drms, guide_f_max_hfr, guide_f_lost_star_to, guide_f_cal_timeout, guide_f_sep_min,
    guide_f_sep_max_ref, guide_f_save_log, guide_f_use_guide_head, guide_f_invent_star, guide_f_latest_checks, guide_f_accuracy_thr, guide_f_gpg_period, guide_f_gpg_estimate_period,
    guide_f_gpg_dark, guide_f_gpg_dark_interval, guide_f_gpg_p_weight, guide_f_gpg_se0_length, guide_f_gpg_se0_signal, guide_f_gpg_pk_length, guide_f_gpg_pk_signal, guide_f_gpg_se1_length,
    guide_f_gpg_se1_signal, guide_f_gpg_points_approx, guide_f_gpg_min_periods_inf, guide_f_gpg_min_periods_period, mosaic_planner_title, mosaic_target_field, mosaic_target_label, mosaic_target_placeholder,
    mosaic_picking, mosaic_repick, mosaic_pick_sky, mosaic_grid_label, mosaic_overlap_label, mosaic_pa_label, mosaic_cam_no_fov, mosaic_kstars_fov_note, mosaic_capture_seq,
    mosaic_filter_col, mosaic_exp_col, mosaic_count_col, mosaic_filter_placeholder, mosaic_add_filter, mosaic_step_track, mosaic_step_focus, mosaic_step_align,
    mosaic_step_guide, mosaic_output, mosaic_output_dir, mosaic_output_placeholder, mosaic_send_scheduler, mosaic_err_no_center, mosaic_err_no_frames, mosaic_err_no_fov, mosaic_scheduler_opts,
    sched_title, sched_job_singular, sched_job_plural, sched_status_idle, sched_status_running, sched_status_paused, sched_status_unknown, sched_status_startup, sched_status_shutdown, sched_status_loading, sched_status_aborted, sched_state_idle,
    sched_state_evaluating, sched_state_scheduled, sched_state_active, sched_state_error, sched_state_aborted, sched_state_invalid, sched_state_complete, sched_stage_slewing,
    sched_stage_slew_done, sched_stage_focusing, sched_stage_focus_done, sched_stage_aligning, sched_stage_align_done, sched_stage_reslewing, sched_stage_reslew_done, sched_stage_post_focus,
    sched_stage_post_focus_done, sched_stage_guiding, sched_stage_guide_done, sched_stage_capturing, sched_stage_done, sched_jobs_section, sched_refresh_jobs, sched_no_jobs,
    sched_col_name, sched_col_coords, sched_col_state, sched_col_alt, sched_col_progress, sched_col_start, sched_col_end, sched_remove_job,
    sched_btn_start, sched_btn_stop, sched_settings_section, sched_greedy, sched_remember_progress, sched_reschedule_error, sched_scripts_section, sched_startup_legend,
    sched_enable_startup, sched_pre_script, sched_post_script, sched_shutdown_legend, sched_enable_shutdown, sched_apply_scripts, sched_add_job_section, sched_target_label,
    sched_target_placeholder, sched_search_catalog, sched_not_found, sched_ra_label, sched_dec_label, sched_min_alt, sched_moon_sep, sched_pa_label,
    sched_steps_legend, sched_step_track, sched_step_focus, sched_step_align, sched_step_guide, sched_start_when, sched_cond_asap, sched_cond_at_time,
    sched_complete_when, sched_cond_seq, sched_cond_repeat, sched_cond_loop, sched_cond_finish_at, sched_times_unit, sched_seq_label, sched_seq_col_type,
    sched_seq_col_filter, sched_seq_col_exp, sched_seq_col_count, sched_add_frame, sched_clear_btn, sched_add_job_btn, sched_err_ra, sched_err_dec,
    sched_err_frames, sky_add_scheduler, sky_create_mosaic, sky_scheduler_jobs,
    // Files tab v2 — browser controls, file actions, livestacker extras.
    files_sort_name, files_sort_date, files_sort_size, files_sort_asc, files_sort_desc, files_filter_all,
    files_filter_images, files_filter_fits, files_filter_jpg, files_filter_placeholder, files_refresh, files_download,
    files_rename, files_delete, files_copy_path, files_confirm_delete, files_rename_prompt, files_section_browser,
    files_section_preview, files_path_copied, files_open_in_files, files_reveal_captures, files_action_menu,
    livestack_section_directories, livestack_section_alignment, livestack_section_stacking, livestack_section_rejection,
    livestack_section_postprocess, livestack_section_calibration, livestack_num_in_mem, livestack_weighting,
    livestack_downscale, livestack_post_process, livestack_sharpen, livestack_denoise, livestack_deconv,
    livestack_master_dark, livestack_master_flat, livestack_open_output, livestack_open_input, livestack_latest_preview,
    livestack_preview_hint, livestack_out_of_sandbox, livestack_align_plate_solve, livestack_align_none,
    livestack_stack_mean, livestack_stack_sigma, livestack_stack_windsor, livestack_stack_imagemm,
    livestack_downscale_none, livestack_downscale_x2, livestack_downscale_x3, livestack_downscale_x4,
    livestack_weighting_equal, livestack_weighting_hfr, livestack_weighting_stars, livestack_reset,
    livestack_min_snr, livestack_max_snr, livestack_compact_label,
    files_subtab_preview, files_subtab_controls, files_subtab_settings, files_folder_hint, files_close_preview,
}

// ── Loader ───────────────────────────────────────────────────────────────────

const EN_JSON: &str = include_str!("en.json");
const FR_JSON: &str = include_str!("fr.json");

fn leak(map: &serde_json::Map<String, serde_json::Value>, key: &str) -> &'static str {
    let s = map
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("i18n: missing or non-string key {key:?}"));
    Box::leak(s.to_owned().into_boxed_str())
}

fn parse(json: &str) -> Translations {
    let value: serde_json::Value = serde_json::from_str(json).expect("i18n json failed to parse");
    let obj = value
        .as_object()
        .expect("i18n json must be a top-level object");
    build(obj)
}

pub fn t(lang: Lang) -> &'static Translations {
    static EN: OnceLock<Translations> = OnceLock::new();
    static FR: OnceLock<Translations> = OnceLock::new();
    match lang {
        Lang::En => EN.get_or_init(|| parse(EN_JSON)),
        Lang::Fr => FR.get_or_init(|| parse(FR_JSON)),
    }
}

// ── Constellation name translations (FR only) ────────────────────────────────

pub fn constellation_name(abbr: &str, lang: Lang) -> Option<&'static str> {
    match lang {
        Lang::En => None,
        Lang::Fr => fr_constellations().get(abbr).copied(),
    }
}

fn fr_constellations() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| {
        let value: serde_json::Value =
            serde_json::from_str(FR_JSON).expect("fr.json failed to parse");
        let table = value
            .get("__constellations__")
            .and_then(|v| v.as_object())
            .expect("fr.json: missing \"__constellations__\" object");
        table
            .iter()
            .map(|(k, v)| {
                let v = v
                    .as_str()
                    .unwrap_or_else(|| panic!("fr.json: constellations[{k:?}] not a string"));
                let k_s: &'static str = Box::leak(k.clone().into_boxed_str());
                let v_s: &'static str = Box::leak(v.to_owned().into_boxed_str());
                (k_s, v_s)
            })
            .collect()
    })
}
