//! Line icons from Tabler Icons (MIT, copyright (c) 2020-2025 Paweł Kuna, https://tabler.io/icons; see NOTICE),
//! the outline set at 24 by 24 with a 2-pixel stroke, inlined so the page loads no icon file. Generated from
//! `@tabler/icons` 3.35.0: only the path data is taken, unchanged.

/// An icon as inline SVG (`class="i"`, hidden from screen readers: the text beside it says what it means).
pub fn icon(name: &str) -> String {
    let paths = match name {
        "message-question" => {
            r#"<path d="M8 9h8"/><path d="M8 13h6"/><path d="M14 18h-1l-5 3v-3h-2a3 3 0 0 1 -3 -3v-8a3 3 0 0 1 3 -3h12a3 3 0 0 1 3 3v4.5"/><path d="M19 22v.01"/><path d="M19 19a2.003 2.003 0 0 0 .914 -3.782a1.98 1.98 0 0 0 -2.414 .483"/>"#
        }
        "key" => {
            r#"<path d="M16.555 3.843l3.602 3.602a2.877 2.877 0 0 1 0 4.069l-2.643 2.643a2.877 2.877 0 0 1 -4.069 0l-.301 -.301l-6.558 6.558a2 2 0 0 1 -1.239 .578l-.175 .008h-1.172a1 1 0 0 1 -.993 -.883l-.007 -.117v-1.172a2 2 0 0 1 .467 -1.284l.119 -.13l.414 -.414h2v-2h2v-2l2.144 -2.144l-.301 -.301a2.877 2.877 0 0 1 0 -4.069l2.643 -2.643a2.877 2.877 0 0 1 4.069 0z"/><path d="M15 9h.01"/>"#
        }
        "lock-open" => {
            r#"<path d="M5 11m0 2a2 2 0 0 1 2 -2h10a2 2 0 0 1 2 2v6a2 2 0 0 1 -2 2h-10a2 2 0 0 1 -2 -2z"/><path d="M12 16m-1 0a1 1 0 1 0 2 0a1 1 0 1 0 -2 0"/><path d="M8 11v-5a4 4 0 0 1 8 0"/>"#
        }
        "plug-x" => {
            r#"<path d="M13.55 17.733a5.806 5.806 0 0 1 -7.356 -4.052a5.81 5.81 0 0 1 1.537 -5.627l2.054 -2.054l7.165 7.165"/><path d="M4 20l3.5 -3.5"/><path d="M15 4l-3.5 3.5"/><path d="M20 9l-3.5 3.5"/><path d="M16 16l4 4"/><path d="M20 16l-4 4"/>"#
        }
        "server-off" => {
            r#"<path d="M12 12h-6a3 3 0 0 1 -3 -3v-2c0 -1.083 .574 -2.033 1.435 -2.56m3.565 -.44h10a3 3 0 0 1 3 3v2a3 3 0 0 1 -3 3h-2"/><path d="M16 12h2a3 3 0 0 1 3 3v2m-1.448 2.568a2.986 2.986 0 0 1 -1.552 .432h-12a3 3 0 0 1 -3 -3v-2a3 3 0 0 1 3 -3h6"/><path d="M7 8v.01"/><path d="M7 16v.01"/><path d="M3 3l18 18"/>"#
        }
        "git-commit" => {
            r#"<path d="M12 12m-3 0a3 3 0 1 0 6 0a3 3 0 1 0 -6 0"/><path d="M12 3l0 6"/><path d="M12 15l0 6"/>"#
        }
        "mail" => {
            r#"<path d="M3 7a2 2 0 0 1 2 -2h14a2 2 0 0 1 2 2v10a2 2 0 0 1 -2 2h-14a2 2 0 0 1 -2 -2v-10z"/><path d="M3 7l9 6l9 -6"/>"#
        }
        "clock" => r#"<path d="M3 12a9 9 0 1 0 18 0a9 9 0 0 0 -18 0"/><path d="M12 7v5l3 3"/>"#,
        "rocket" => {
            r#"<path d="M4 13a8 8 0 0 1 7 7a6 6 0 0 0 3 -5a9 9 0 0 0 6 -8a3 3 0 0 0 -3 -3a9 9 0 0 0 -8 6a6 6 0 0 0 -5 3"/><path d="M7 14a6 6 0 0 0 -3 6a6 6 0 0 0 6 -3"/><path d="M15 9m-1 0a1 1 0 1 0 2 0a1 1 0 1 0 -2 0"/>"#
        }
        "activity" => r#"<path d="M3 12h4l3 8l4 -16l3 8h4"/>"#,
        "bolt" => r#"<path d="M13 3l0 7l6 0l-8 11l0 -7l-6 0l8 -11"/>"#,
        "server" => {
            r#"<path d="M3 4m0 3a3 3 0 0 1 3 -3h12a3 3 0 0 1 3 3v2a3 3 0 0 1 -3 3h-12a3 3 0 0 1 -3 -3z"/><path d="M3 12m0 3a3 3 0 0 1 3 -3h12a3 3 0 0 1 3 3v2a3 3 0 0 1 -3 3h-12a3 3 0 0 1 -3 -3z"/><path d="M7 8l0 .01"/><path d="M7 16l0 .01"/>"#
        }
        "terminal-2" => {
            r#"<path d="M8 9l3 3l-3 3"/><path d="M13 15l3 0"/><path d="M3 4m0 2a2 2 0 0 1 2 -2h14a2 2 0 0 1 2 2v12a2 2 0 0 1 -2 2h-14a2 2 0 0 1 -2 -2z"/>"#
        }
        "circle-check" => r#"<path d="M12 12m-9 0a9 9 0 1 0 18 0a9 9 0 1 0 -18 0"/><path d="M9 12l2 2l4 -4"/>"#,
        "webhook" => {
            r#"<path d="M4.876 13.61a4 4 0 1 0 6.124 3.39h6"/><path d="M15.066 20.502a4 4 0 1 0 1.934 -7.502c-.706 0 -1.424 .179 -2 .5l-3 -5.5"/><path d="M16 8a4 4 0 1 0 -8 0c0 1.506 .77 2.818 2 3.5l-3 5.5"/>"#
        }
        "arrow-right" => r#"<path d="M5 12l14 0"/><path d="M13 18l6 -6"/><path d="M13 6l6 6"/>"#,
        "check" => r#"<path d="M5 12l5 5l10 -10"/>"#,
        "x" => r#"<path d="M18 6l-12 12"/><path d="M6 6l12 12"/>"#,
        "alert-triangle" => {
            r#"<path d="M12 9v4"/><path d="M10.363 3.591l-8.106 13.534a1.914 1.914 0 0 0 1.636 2.871h16.214a1.914 1.914 0 0 0 1.636 -2.87l-8.106 -13.536a1.914 1.914 0 0 0 -3.274 0z"/><path d="M12 16h.01"/>"#
        }
        "git-pull-request" => {
            r#"<path d="M6 18m-2 0a2 2 0 1 0 4 0a2 2 0 1 0 -4 0"/><path d="M6 6m-2 0a2 2 0 1 0 4 0a2 2 0 1 0 -4 0"/><path d="M18 18m-2 0a2 2 0 1 0 4 0a2 2 0 1 0 -4 0"/><path d="M6 8l0 8"/><path d="M11 6h5a2 2 0 0 1 2 2v8"/><path d="M14 9l-3 -3l3 -3"/>"#
        }
        "loader-2" => r#"<path d="M12 3a9 9 0 1 0 9 9"/>"#,
        "player-pause" => {
            r#"<path d="M6 5m0 1a1 1 0 0 1 1 -1h2a1 1 0 0 1 1 1v12a1 1 0 0 1 -1 1h-2a1 1 0 0 1 -1 -1z"/><path d="M14 5m0 1a1 1 0 0 1 1 -1h2a1 1 0 0 1 1 1v12a1 1 0 0 1 -1 1h-2a1 1 0 0 1 -1 -1z"/>"#
        }
        "hand-stop" => {
            r#"<path d="M8 13v-7.5a1.5 1.5 0 0 1 3 0v6.5"/><path d="M11 5.5v-2a1.5 1.5 0 1 1 3 0v8.5"/><path d="M14 5.5a1.5 1.5 0 0 1 3 0v6.5"/><path d="M17 7.5a1.5 1.5 0 0 1 3 0v8.5a6 6 0 0 1 -6 6h-2h.208a6 6 0 0 1 -5.012 -2.7a69.74 69.74 0 0 1 -.196 -.3c-.312 -.479 -1.407 -2.388 -3.286 -5.728a1.5 1.5 0 0 1 .536 -2.022a1.867 1.867 0 0 1 2.28 .28l1.47 1.47"/>"#
        }
        "list-details" => {
            r#"<path d="M13 5h8"/><path d="M13 9h5"/><path d="M13 15h8"/><path d="M13 19h5"/><path d="M3 4m0 1a1 1 0 0 1 1 -1h4a1 1 0 0 1 1 1v4a1 1 0 0 1 -1 1h-4a1 1 0 0 1 -1 -1z"/><path d="M3 14m0 1a1 1 0 0 1 1 -1h4a1 1 0 0 1 1 1v4a1 1 0 0 1 -1 1h-4a1 1 0 0 1 -1 -1z"/>"#
        }
        "topology-star-3" => {
            r#"<path d="M10 19a2 2 0 1 0 -4 0a2 2 0 0 0 4 0z"/><path d="M18 5a2 2 0 1 0 -4 0a2 2 0 0 0 4 0z"/><path d="M10 5a2 2 0 1 0 -4 0a2 2 0 0 0 4 0z"/><path d="M6 12a2 2 0 1 0 -4 0a2 2 0 0 0 4 0z"/><path d="M18 19a2 2 0 1 0 -4 0a2 2 0 0 0 4 0z"/><path d="M14 12a2 2 0 1 0 -4 0a2 2 0 0 0 4 0z"/><path d="M22 12a2 2 0 1 0 -4 0a2 2 0 0 0 4 0z"/><path d="M6 12h4"/><path d="M14 12h4"/><path d="M15 7l-2 3"/><path d="M9 7l2 3"/><path d="M11 14l-2 3"/><path d="M13 14l2 3"/>"#
        }
        "gauge" => {
            r#"<path d="M12 12m-9 0a9 9 0 1 0 18 0a9 9 0 1 0 -18 0"/><path d="M12 12m-1 0a1 1 0 1 0 2 0a1 1 0 1 0 -2 0"/><path d="M13.41 10.59l2.59 -2.59"/><path d="M7 12a5 5 0 0 1 5 -5"/>"#
        }
        "inbox" => {
            r#"<path d="M4 4m0 2a2 2 0 0 1 2 -2h12a2 2 0 0 1 2 2v12a2 2 0 0 1 -2 2h-12a2 2 0 0 1 -2 -2z"/><path d="M4 13h3l3 3h4l3 -3h3"/>"#
        }
        "logout" => {
            r#"<path d="M14 8v-2a2 2 0 0 0 -2 -2h-7a2 2 0 0 0 -2 2v12a2 2 0 0 0 2 2h7a2 2 0 0 0 2 -2v-2"/><path d="M9 12h12l-3 -3"/><path d="M18 15l3 -3"/>"#
        }
        "chevron-right" => r#"<path d="M9 6l6 6l-6 6"/>"#,
        _ => "",
    };
    format!(r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true">{paths}</svg>"#)
}
