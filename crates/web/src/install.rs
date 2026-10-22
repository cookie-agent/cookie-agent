//! Installing the page as an app (spec 9), independent of the browser:
//! which offer the page makes from what the browser says about itself.
//!
//! Chromium hands the page a prompt (`beforeinstallprompt`) once the page
//! qualifies, which the page shows as **Install app**. iOS and iPadOS have
//! no prompt: the page says how to add it from the Share menu instead. A
//! page that is not a secure context (plain HTTP on a LAN address) cannot
//! be installed at all, and a page already running as an app offers
//! nothing.

/// The `localStorage` key of the phone's install banner: `dismissed` once
/// the user closed it.
pub const BANNER_KEY: &str = "cookie.install_banner";

/// What the browser says about installing the page.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InstallContext {
    /// The page is a secure context: HTTPS, or `localhost` and `127.0.0.1`.
    pub secure: bool,
    /// The page runs as an installed app: `display-mode: standalone`, or
    /// iOS's `navigator.standalone`.
    pub standalone: bool,
    /// The browser said it installed the page (`appinstalled`).
    pub installed: bool,
    /// The browser handed the page an install prompt it has not used.
    pub prompt: bool,
    /// The browser runs on iOS or iPadOS.
    pub ios: bool,
}

/// What the page offers for installing it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallOffer {
    /// It runs as an app, or the browser just installed it: nothing.
    Installed,
    /// **Install app**, which opens the browser's prompt.
    Prompt,
    /// Steps for iOS: Share, then Add to Home Screen.
    AddToHomeScreen,
    /// Installing needs HTTPS.
    NeedsHttps,
    /// The browser offered no prompt (yet): its own menu may install it.
    BrowserMenu,
}

impl InstallContext {
    /// The offer the page makes.
    pub const fn offer(self) -> InstallOffer {
        if self.standalone || self.installed {
            InstallOffer::Installed
        } else if self.prompt {
            InstallOffer::Prompt
        } else if !self.secure {
            InstallOffer::NeedsHttps
        } else if self.ios {
            InstallOffer::AddToHomeScreen
        } else {
            InstallOffer::BrowserMenu
        }
    }
}

impl InstallOffer {
    /// Whether the page has an action for it outside Settings (the connect
    /// screen's, and the phone's banner): a prompt, or iOS's steps.
    pub const fn actionable(self) -> bool {
        matches!(self, Self::Prompt | Self::AddToHomeScreen)
    }
}

/// Whether a browser of `user_agent`, whose screen takes
/// `max_touch_points` touches, runs on iOS or iPadOS. Every iOS browser is
/// Safari underneath, whose Share menu adds the page to the home screen.
/// iPadOS asks for desktop pages by default and names itself a Mac, which
/// has no touch screen.
pub fn is_ios(user_agent: &str, max_touch_points: i32) -> bool {
    ["iPhone", "iPad", "iPod"]
        .iter()
        .any(|device| user_agent.contains(device))
        || (user_agent.contains("Macintosh") && max_touch_points > 1)
}

/// Whether a phone shows the install banner: the page has an action for
/// installing it and the user has not dismissed the banner.
pub const fn shows_banner(offer: InstallOffer, phone: bool, dismissed: bool) -> bool {
    phone && offer.actionable() && !dismissed
}

/// Whether the value stored under [`BANNER_KEY`] dismissed the banner.
pub fn banner_dismissed(stored: Option<&str>) -> bool {
    stored == Some("dismissed")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECURE: InstallContext = InstallContext {
        secure: true,
        standalone: false,
        installed: false,
        prompt: false,
        ios: false,
    };

    #[test]
    fn the_app_and_the_page_are_named_cookie_agent() {
        let manifest: serde_json::Value =
            serde_json::from_str(include_str!("../assets/manifest.webmanifest")).unwrap();
        assert_eq!(manifest["name"], "Cookie Agent");
        assert_eq!(manifest["short_name"], "Cookie Agent");
        let index = include_str!("../index.html");
        assert!(index.contains("<title>Cookie Agent</title>"));
    }

    #[test]
    fn a_page_running_as_an_app_offers_nothing() {
        for context in [
            InstallContext {
                standalone: true,
                prompt: true,
                ..SECURE
            },
            InstallContext {
                installed: true,
                ios: true,
                ..SECURE
            },
            InstallContext {
                standalone: true,
                secure: false,
                ..SECURE
            },
        ] {
            assert_eq!(context.offer(), InstallOffer::Installed, "{context:?}");
        }
    }

    #[test]
    fn a_prompt_is_offered_where_the_browser_gave_one() {
        let context = InstallContext {
            prompt: true,
            ..SECURE
        };
        assert_eq!(context.offer(), InstallOffer::Prompt);
        assert!(context.offer().actionable());
    }

    #[test]
    fn ios_gets_steps_and_other_browsers_their_menu() {
        let ios = InstallContext {
            ios: true,
            ..SECURE
        };
        assert_eq!(ios.offer(), InstallOffer::AddToHomeScreen);
        assert!(ios.offer().actionable());
        assert_eq!(SECURE.offer(), InstallOffer::BrowserMenu);
        assert!(!SECURE.offer().actionable());
    }

    #[test]
    fn a_page_that_is_not_secure_cannot_be_installed() {
        for ios in [false, true] {
            let context = InstallContext {
                secure: false,
                ios,
                ..SECURE
            };
            assert_eq!(context.offer(), InstallOffer::NeedsHttps, "{context:?}");
            assert!(!context.offer().actionable());
        }
    }

    #[test]
    fn ios_and_ipados_are_recognised_by_their_agent() {
        let iphone = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Mobile/15E148 Safari/604.1";
        let chrome_on_iphone = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/131.0.6778.73 Mobile/15E148 Safari/604.1";
        let ipad_desktop = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Safari/605.1.15";
        let android = "Mozilla/5.0 (Linux; Android 15; Pixel 9) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Mobile Safari/537.36";
        assert!(is_ios(iphone, 5));
        assert!(is_ios(chrome_on_iphone, 5));
        assert!(is_ios(ipad_desktop, 5));
        // The same agent on a Mac, which has no touch screen.
        assert!(!is_ios(ipad_desktop, 0));
        assert!(!is_ios(android, 5));
    }

    /// The page's own files, as Trunk copies them into the bundle.
    fn asset(path: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
        std::fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    }

    /// The width and height of a PNG, or of a WebP of a lossy (`VP8 `) or
    /// extended (`VP8X`) kind.
    fn image_size(bytes: &[u8]) -> (u32, u32) {
        let u32_be = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
        let u24_le = |at: usize| {
            u32::from(bytes[at]) | u32::from(bytes[at + 1]) << 8 | u32::from(bytes[at + 2]) << 16
        };
        if bytes.starts_with(b"\x89PNG") {
            (u32_be(16), u32_be(20))
        } else if bytes.starts_with(b"RIFF") && &bytes[12..16] == b"VP8 " {
            (u24_le(26) & 0x3fff, u24_le(28) & 0x3fff)
        } else if bytes.starts_with(b"RIFF") && &bytes[12..16] == b"VP8X" {
            (u24_le(24) + 1, u24_le(27) + 1)
        } else {
            panic!("not a PNG or a WebP this test reads");
        }
    }

    /// What Chromium's install criteria ask of the manifest, and its richer
    /// install dialog of the screenshots; the files it names are the sizes
    /// it says.
    #[test]
    fn the_manifest_meets_the_install_criteria() {
        let manifest: serde_json::Value =
            serde_json::from_slice(&asset("assets/manifest.webmanifest")).expect("JSON");
        for field in ["name", "short_name", "description"] {
            assert!(
                manifest[field]
                    .as_str()
                    .is_some_and(|value| !value.is_empty())
            );
        }
        // Relative to the manifest, so the page installs from any path.
        for field in ["id", "start_url", "scope"] {
            assert_eq!(manifest[field], "./", "{field}");
        }
        assert_eq!(manifest["display"], "standalone");
        assert!(manifest.get("prefer_related_applications").is_none());
        // The light theme's colours, as `index.html` names them.
        let index = String::from_utf8(asset("index.html")).unwrap();
        for field in ["background_color", "theme_color"] {
            let colour = manifest[field].as_str().expect(field);
            assert!(
                index.contains(&format!(
                    r#"media="(prefers-color-scheme: light)" content="{colour}""#
                )),
                "{field} {colour}"
            );
        }
        let icons = manifest["icons"].as_array().expect("icons");
        for (size, side) in [("192x192", 192), ("512x512", 512)] {
            for purpose in ["any", "maskable"] {
                let icon = icons
                    .iter()
                    .find(|icon| icon["sizes"] == size && icon["purpose"] == purpose)
                    .unwrap_or_else(|| panic!("a {size} {purpose} icon"));
                assert_eq!(icon["type"], "image/png");
                let src = icon["src"].as_str().unwrap();
                assert_eq!(
                    image_size(&asset(&format!("assets/{src}"))),
                    (side, side),
                    "{src}"
                );
            }
        }
        let screenshots = manifest["screenshots"].as_array().expect("screenshots");
        for form_factor in ["narrow", "wide"] {
            assert!(
                screenshots
                    .iter()
                    .any(|shot| shot["form_factor"] == form_factor),
                "a {form_factor} screenshot"
            );
        }
        for shot in screenshots {
            let src = shot["src"].as_str().unwrap();
            let (width, height) = image_size(&asset(&format!("assets/{src}")));
            assert_eq!(shot["sizes"], format!("{width}x{height}"), "{src}");
            assert_eq!(shot["type"], "image/webp", "{src}");
            assert!(
                shot["label"]
                    .as_str()
                    .is_some_and(|label| !label.is_empty())
            );
            // Chromium shows screenshots 320 to 3840 px a side, the longer
            // at most 2.3 times the shorter.
            for side in [width, height] {
                assert!((320..=3840).contains(&side), "{src}: {width}x{height}");
            }
            let (long, short) = (width.max(height), width.min(height));
            assert!(f64::from(long) <= 2.3 * f64::from(short), "{src}");
        }
    }

    /// The service worker caches nothing, takes over at once, keeps off the
    /// daemon's WebSocket endpoints, and carries the line the build stamps;
    /// `index.html` copies it under its own name and names the manifest,
    /// iOS's tags, and the early keeper of Chromium's install prompt.
    #[test]
    fn the_service_worker_is_network_only() {
        let worker = String::from_utf8(asset("assets/sw.js")).unwrap();
        assert!(worker.contains("self.skipWaiting()"));
        assert!(worker.contains("self.clients.claim()"));
        assert!(worker.contains(r#"addEventListener("fetch""#));
        // A failed fetch becomes the network error, not an uncaught rejection.
        assert!(worker.contains("event.respondWith(fetch(request).catch(() => Response.error()))"));
        assert!(worker.contains(r"/\/(ws|runtime)$/"));
        // No Cache Storage: it caches nothing.
        assert!(!worker.contains("caches."));
        assert_eq!(worker.lines().last(), Some("// build: source"));
        let index = String::from_utf8(asset("index.html")).unwrap();
        for needle in [
            r#"<link data-trunk rel="copy-file" href="assets/sw.js" />"#,
            r#"<link rel="manifest" href="manifest.webmanifest" />"#,
            r#"<link rel="apple-touch-icon" href="icons/apple-touch-icon.png" />"#,
            r#"<meta name="apple-mobile-web-app-capable" content="yes" />"#,
            r#"<meta name="apple-mobile-web-app-title" content="Cookie Agent" />"#,
            r#"addEventListener("beforeinstallprompt""#,
            "event.preventDefault()",
        ] {
            assert!(index.contains(needle), "{needle}");
        }
    }

    #[test]
    fn the_banner_shows_on_a_phone_until_dismissed() {
        assert!(shows_banner(InstallOffer::Prompt, true, false));
        assert!(shows_banner(InstallOffer::AddToHomeScreen, true, false));
        assert!(!shows_banner(InstallOffer::Prompt, true, true));
        assert!(!shows_banner(InstallOffer::Prompt, false, false));
        assert!(!shows_banner(InstallOffer::BrowserMenu, true, false));
        assert!(!shows_banner(InstallOffer::NeedsHttps, true, false));
        assert!(!shows_banner(InstallOffer::Installed, true, false));
        assert!(banner_dismissed(Some("dismissed")));
        assert!(!banner_dismissed(Some("other")));
        assert!(!banner_dismissed(None));
    }
}
