use crate::{command, Result};
use serde_json::{json, Value};

fn destination(verb: &str) -> Result<&'static str> {
    match verb {
        "open-repository" => Ok("https://github.com/xiyunmn/OPLUS-ChargeGuard"),
        "open-author" => Ok("https://github.com/xiyunmn"),
        _ => Err("unknown_browser_destination".into()),
    }
}

fn browser_package(output: &str) -> Option<&str> {
    let package = output.trim();
    // The role has at most one holder. Reject errors, multiple lines and malformed names.
    (package.contains('.')
        && package.split('.').all(|part| {
            !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        }))
    .then_some(package)
}

fn launch_args<'a>(url: &'a str, user: &'a str, package: Option<&'a str>) -> Vec<&'a str> {
    let mut args = vec![
        "start",
        "-W",
        "--user",
        user,
        "-a",
        "android.intent.action.VIEW",
        "-c",
        "android.intent.category.BROWSABLE",
        "-d",
        url,
    ];
    if let Some(package) = package {
        args.extend(["-p", package]);
    } else {
        // Without a configured default, ask Android to resolve browser applications only.
        args.extend([
            "--selector",
            "-a",
            "android.intent.action.MAIN",
            "-c",
            "android.intent.category.APP_BROWSER",
        ]);
    }
    args
}

pub fn open(verb: &str) -> Result<Value> {
    let url = destination(verb)?;
    let user = command::text(
        "/system/bin/cmd",
        &["activity", "get-current-user"],
        3000,
        128,
    )?
    .parse::<u32>()
    .map_err(|_| "current_android_user_unknown")?
    .to_string();
    let role = command::text(
        "/system/bin/cmd",
        &[
            "role",
            "get-role-holders",
            "--user",
            &user,
            "android.app.role.BROWSER",
        ],
        3000,
        4096,
    )
    .unwrap_or_default();
    let package = browser_package(&role);
    let output = command::text(
        "/system/bin/am",
        &launch_args(url, &user, package),
        10000,
        8192,
    )?;
    // am can print an error while returning exit status zero. Require its -W result.
    if !output.lines().any(|line| line.trim() == "Status: ok") {
        return Err("system_browser_launch_unconfirmed".into());
    }
    Ok(json!({"opened":true,"url":url,"browser_package":package,"user":user}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_fixed_destinations_are_accepted() {
        assert!(destination("https://other.example").is_err());
        assert_eq!(
            destination("open-author").unwrap(),
            "https://github.com/xiyunmn"
        );
        assert!(destination("open-repository")
            .unwrap()
            .ends_with("/OPLUS-ChargeGuard"));
    }

    #[test]
    fn invalid_role_output_cannot_become_a_package() {
        for output in [
            "",
            "Error: role not found",
            "com.browser\ncom.other",
            "com.browser;id",
            ".browser",
            "com..browser",
        ] {
            assert_eq!(browser_package(output), None);
        }
        assert_eq!(
            browser_package(" com.android.chrome\n"),
            Some("com.android.chrome")
        );
    }

    #[test]
    fn default_browser_is_targeted_and_fallback_resolves_only_browsers() {
        let url = destination("open-repository").unwrap();
        let args = launch_args(url, "10", Some("com.android.chrome"));
        assert_eq!(&args[args.len() - 2..], &["-p", "com.android.chrome"]);
        assert!(args.windows(2).any(|p| p == ["--user", "10"]));
        assert!(!args.contains(&"--selector"));
        let args = launch_args(url, "0", None);
        assert!(args.contains(&"--selector"));
        assert_eq!(args.last(), Some(&"android.intent.category.APP_BROWSER"));
        assert!(args.windows(2).any(|p| p == ["-d", url]));
    }
}
