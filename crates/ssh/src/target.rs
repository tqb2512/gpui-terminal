use std::{
    fmt,
    net::{IpAddr, Ipv6Addr},
    path::PathBuf,
};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Endpoint {
    host: String,
    port: u16,
}

impl Endpoint {
    pub fn new(host: &str, port: u16) -> Result<Self, TargetError> {
        if port == 0 {
            return Err(TargetError::Port);
        }
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(Self {
                host: ip.to_string(),
                port,
            });
        }
        let dns = host.strip_suffix('.').unwrap_or(host);
        if dns.len() > 253
            || !dns.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
        {
            return Err(TargetError::Host);
        }
        // Some resolvers treat noncanonical dotted decimal as an IP.
        if dns
            .split('.')
            .all(|label| label.bytes().all(|byte| byte.is_ascii_digit()))
        {
            return Err(TargetError::Host);
        }
        Ok(Self {
            host: host.to_ascii_lowercase(),
            port,
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Target {
    endpoint: Endpoint,
    username: String,
}

impl Target {
    pub fn new(endpoint: Endpoint, username: &str) -> Result<Self, TargetError> {
        if username.is_empty()
            || username.len() > 255
            || username
                .chars()
                .any(|ch| ch == '@' || ch.is_whitespace() || ch.is_control())
        {
            return Err(TargetError::Username);
        }
        Ok(Self {
            endpoint,
            username: username.into(),
        })
    }

    pub fn parse(input: &str) -> Result<Self, TargetError> {
        let (username, address) = input.trim().split_once('@').ok_or(TargetError::Username)?;
        let (host, port) = if let Some(address) = address.strip_prefix('[') {
            let (host, rest) = address.split_once(']').ok_or(TargetError::Host)?;
            host.parse::<Ipv6Addr>().map_err(|_| TargetError::Host)?;
            let port = if rest.is_empty() {
                22
            } else {
                parse_port(rest.strip_prefix(':').ok_or(TargetError::Port)?)?
            };
            (host, port)
        } else if address.parse::<Ipv6Addr>().is_ok() {
            (address, 22)
        } else if let Some((host, port)) = address.split_once(':') {
            (host, parse_port(port)?)
        } else {
            (address, 22)
        };
        Self::new(Endpoint::new(host, port)?, username)
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
    pub fn username(&self) -> &str {
        &self.username
    }
}

fn parse_port(port: &str) -> Result<u16, TargetError> {
    if !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(TargetError::Port);
    }
    port.parse().map_err(|_| TargetError::Port)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthMethod {
    Password,
    PrivateKey { path: Option<PathBuf> },
}

#[derive(Debug, thiserror::Error)]
pub enum TargetError {
    #[error("Enter an SSH username in user@host form.")]
    Username,
    #[error("Enter a valid DNS name or IP address.")]
    Host,
    #[error("Enter a port between 1 and 65535.")]
    Port,
}
