use std::{str::FromStr, time::Duration};

use regex::Regex;
use tokio::runtime::Runtime;
use trust_dns_resolver::IntoName;
use trust_dns_server::client::rr::Name;
use zerotier_central_api::apis::configuration::Configuration;

use anyhow::anyhow;

use crate::authority::Authority;
use crate::authority::PtrAuthority;
use crate::authority::ZTAuthority;

pub(crate) const DOMAIN_NAME: &str = "domain.";
pub(crate) const VERSION_STRING: &str = env!("CARGO_PKG_VERSION");

fn version() -> String {
    "zeronsd ".to_string() + VERSION_STRING
}

pub(crate) fn central_config(token: String) -> Configuration {
    let mut config = Configuration::default();
    config.user_agent = Some(version());
    config.bearer_access_token = Some(token);
    return config;
}

pub(crate) fn init_runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(num_cpus::get())
        .thread_name("zeronsd")
        .build()
        .expect("failed to initialize tokio")
}

pub(crate) fn parse_ip_from_cidr(ip_with_cidr: String) -> String {
    ip_with_cidr
        .splitn(2, "/")
        .next()
        .expect("Could not parse IP from CIDR")
        .to_string()
}

pub(crate) fn central_token(arg: Option<&str>) -> Option<String> {
    if arg.is_some() {
        return Some(
            std::fs::read_to_string(arg.unwrap())
                .expect("Could not load token file")
                .trim()
                .to_string(),
        );
    }

    if let Ok(token) = std::env::var("ZEROTIER_CENTRAL_TOKEN") {
        if token.len() > 0 {
            return Some(token);
        }
    }

    None
}

pub(crate) fn authtoken_path(arg: Option<&str>) -> String {
    if let Some(arg) = arg {
        return String::from(arg);
    } else {
        if cfg!(target_os = "linux") {
            String::from("/var/lib/zerotier-one/authtoken.secret")
        } else if cfg!(target_os = "windows") {
            String::from("C:/ProgramData/ZeroTier/One/authtoken.secret")
        } else if cfg!(target_os = "macos") {
            String::from("/Library/Application Support/ZeroTier/One/authtoken.secret")
        } else {
            panic!(
                "authtoken.secret not found; please provide the -s option to provide a custom path"
            )
        }
    }
}

pub(crate) fn domain_or_default(tld: Option<&str>) -> Result<Name, anyhow::Error> {
    if let Some(tld) = tld {
        if tld.len() > 0 {
            return Ok(Name::from_str(&format!("{}.", tld))?);
        } else {
            return Err(anyhow!("Domain name must not be empty if provided."));
        }
    };

    Ok(Name::from_str(DOMAIN_NAME)?)
}

pub(crate) fn parse_member_name(name: Option<String>, domain_name: Name) -> Option<Name> {
    if let Some(name) = name {
        let name = name.trim();
        if name.len() > 0 {
            match name.to_fqdn(domain_name) {
                Ok(record) => return Some(record),
                Err(e) => {
                    eprintln!("Record {} not entered into catalog: {:?}", name, e);
                    return None;
                }
            };
        }
    }

    None
}

pub(crate) async fn get_listen_ips(
    authtoken_path: &str,
    network_id: &str,
) -> Result<Vec<String>, anyhow::Error> {
    let authtoken = std::fs::read_to_string(authtoken_path)?;
    let mut configuration = zerotier_one_api::apis::configuration::Configuration::default();
    let api_key = zerotier_one_api::apis::configuration::ApiKey {
        prefix: None,
        key: authtoken,
    };

    configuration.user_agent = Some(version());
    configuration.api_key = Some(api_key);

    let listen =
        zerotier_one_api::apis::network_api::get_network(&configuration, network_id).await?;
    if let Some(assigned) = listen.assigned_addresses {
        if assigned.len() > 0 {
            return Ok(assigned);
        }
    }

    Err(anyhow!("No listen IPs available on this network"))
}

/*
 * FIXME this and init_authority need an overhaul
 */

pub(crate) fn update_central_dns(
    runtime: &mut Runtime,
    domain_name: Name,
    ip: String,
    token: String,
    network: String,
) -> Result<(), anyhow::Error> {
    let config = central_config(token);

    let mut zt_network = runtime.block_on(
        zerotier_central_api::apis::network_api::get_network_by_id(&config, &network),
    )?;

    let mut domain_name = domain_name.clone();
    domain_name.set_fqdn(false);

    let dns = Some(Box::new(zerotier_central_api::models::NetworkConfigDns {
        domain: Some(domain_name.to_string()),
        servers: Some(Vec::from([String::from(ip.clone())])),
    }));

    if let Some(mut zt_network_config) = zt_network.config.to_owned() {
        zt_network_config.dns = dns;
        zt_network.config = Some(zt_network_config);
        runtime.block_on(zerotier_central_api::apis::network_api::update_network(
            &config, &network, zt_network,
        ))?;
    }

    Ok(())
}

pub(crate) fn init_authority(
    ptr_authority: PtrAuthority,
    token: String,
    network: String,
    domain_name: Name,
    hosts_file: Option<String>,
    update_interval: Duration,
    authority: Authority,
) -> ZTAuthority {
    ZTAuthority::new(
        domain_name.clone(),
        network.clone(),
        central_config(token),
        hosts_file,
        ptr_authority,
        update_interval,
        authority,
    )
}

fn translation_table() -> Vec<(Regex, &'static str)> {
    vec![
        (Regex::new(r"\s+").unwrap(), "-"), // translate whitespace to `-`
        (Regex::new(r"[^.\s\w\d-]+").unwrap(), ""), // catch-all at the end
    ]
}

pub(crate) trait ToHostname {
    fn to_hostname(self) -> Result<Name, anyhow::Error>;
    fn to_fqdn(self, domain: Name) -> Result<Name, anyhow::Error>;
}

impl ToHostname for &str {
    fn to_hostname(self) -> Result<Name, anyhow::Error> {
        self.clone().to_string().to_hostname()
    }

    fn to_fqdn(self, domain: Name) -> Result<Name, anyhow::Error> {
        Ok(self.to_hostname()?.append_domain(&domain))
    }
}

impl ToHostname for String {
    fn to_hostname(self) -> Result<Name, anyhow::Error> {
        let mut s = self.clone().trim().to_string();
        for (regex, replacement) in translation_table() {
            s = regex.replace_all(&s, replacement).to_string();
        }

        let s = s.trim();

        if s == "." || s.ends_with(".") {
            return Err(anyhow!("Record {} not entered into catalog: '.' and records that ends in '.' are disallowed", s));
        }

        if s.len() == 0 {
            return Err(anyhow!("translated hostname {} is an empty string", self));
        }

        Ok(s.trim().into_name()?)
    }

    fn to_fqdn(self, domain: Name) -> Result<Name, anyhow::Error> {
        Ok(self.to_hostname()?.append_domain(&domain))
    }
}