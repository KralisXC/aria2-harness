//! rustls ClientConfig：`with_server_name(域名)`，证书校验根走 webpki-roots。
//!
//! Phase 3 对齐口径（`docs/c3/Phase3-tls-diff.md`）：
//! - `--enable-http2=true`（默认）时 ALPN 宣告 `h2, http/1.1`；false 时只宣告 `http/1.1`
//!   （Q6 I4：握手前撤掉 h2，而不是协商后忽略）；
//! - `--check-certificate=false` 时用 `NoVerifier` 放行（对齐 aria2 同名选项）；
//! - `--certificate` / `--private-key` 为 PEM 客户端证书，私钥缺省复用证书文件
//!   （aria2 帮助文本：private key 由 --private-key 指定，未指定时从证书文件读取）；
//! - SNI 用 URL 的域名，TCP 可连任意解析出的 IP（Q9）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use webpki_roots::TLS_SERVER_ROOTS;

use crate::error::NetworkError;

/// TLS 相关的任务选项（由 RPC 层从 aria2 同名选项解析）。
#[derive(Debug, Clone)]
pub struct TlsOptions {
    /// `--check-certificate`（默认 true）。
    pub check_certificate: bool,
    /// `--enable-http2`（默认 true）。
    pub enable_http2: bool,
    /// `--certificate`：PEM 客户端证书（可含链）。
    pub client_certificate: Option<PathBuf>,
    /// `--private-key`：PEM 私钥；缺省时从 `client_certificate` 读取。
    pub private_key: Option<PathBuf>,
}

impl Default for TlsOptions {
    fn default() -> Self {
        Self {
            check_certificate: true,
            enable_http2: true,
            client_certificate: None,
            private_key: None,
        }
    }
}

impl TlsOptions {
    /// 代理隧道 / HTTP/1.1 专用：ALPN 只保留 `http/1.1`（Q6 I5）。
    pub fn http1_only(&self) -> Self {
        Self {
            enable_http2: false,
            ..self.clone()
        }
    }

    /// 池键的一部分：不同 TLS 身份不能共用连接。
    pub fn fingerprint(&self) -> String {
        format!(
            "cc={};cert={};key={}",
            self.check_certificate,
            self.client_certificate
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            self.private_key
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        )
    }
}

/// 构造客户端 TLS 配置。
pub fn client_config(options: &TlsOptions) -> Result<Arc<ClientConfig>, NetworkError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|err| NetworkError::CertError(err.to_string()))?;

    let verifier_builder = if options.check_certificate {
        // Phase 4：信任根走操作系统证书存储（Windows CurrentUser/LocalMachine Root），
        // 解决企业内网自建 CA 不在 webpki 内置根里的问题。平台验证器不可用时
        // 回退 webpki-roots（保持 Phase 3 行为）。
        match rustls_platform_verifier::Verifier::new(provider.clone()) {
            Ok(verifier) => builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier)),
            Err(err) => {
                tracing::warn!(%err, "系统证书存储不可用，回退 webpki-roots");
                let mut roots = RootCertStore::empty();
                roots.extend(TLS_SERVER_ROOTS.iter().cloned());
                builder.with_root_certificates(roots)
            }
        }
    } else {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier(provider)))
    };

    // 客户端证书：证书链来自 --certificate，私钥优先 --private-key。
    let mut config = match options.client_certificate.as_deref() {
        Some(cert_path) => {
            let certs = load_certs(cert_path)?;
            if certs.is_empty() {
                return Err(NetworkError::CertError(format!(
                    "客户端证书 `{}` 中没有任何证书",
                    cert_path.display()
                )));
            }
            let key_path = options.private_key.as_deref().unwrap_or(cert_path);
            let key = load_private_key(key_path)?;
            verifier_builder
                .with_client_auth_cert(certs, key)
                .map_err(|err| NetworkError::CertError(err.to_string()))?
        }
        None => {
            if options.private_key.is_some() {
                return Err(NetworkError::CertError(
                    "--private-key 必须与 --certificate 一起使用".into(),
                ));
            }
            verifier_builder.with_no_client_auth()
        }
    };

    config.alpn_protocols = if options.enable_http2 {
        vec![b"h2".to_vec(), b"http/1.1".to_vec()]
    } else {
        vec![b"http/1.1".to_vec()]
    };
    Ok(Arc::new(config))
}

/// 读取 PEM 证书链。
fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, NetworkError> {
    let file = std::fs::File::open(path).map_err(|err| {
        NetworkError::CertError(format!("打开客户端证书 `{}` 失败：{err}", path.display()))
    })?;
    let mut reader = std::io::BufReader::new(file);
    rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| {
            NetworkError::CertError(format!("解析客户端证书 `{}` 失败：{err}", path.display()))
        })
}

/// 读取 PEM 私钥（PKCS#8 / PKCS#1 / SEC1 均可，与 rustls-pemfile 支持面一致）。
fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, NetworkError> {
    let file = std::fs::File::open(path).map_err(|err| {
        NetworkError::CertError(format!("打开私钥 `{}` 失败：{err}", path.display()))
    })?;
    let mut reader = std::io::BufReader::new(file);
    rustls_pemfile::private_key(&mut reader)
        .map_err(|err| NetworkError::CertError(format!("解析私钥 `{}` 失败：{err}", path.display())))?
        .ok_or_else(|| {
            NetworkError::CertError(format!("私钥 `{}` 中没有 PEM 私钥块", path.display()))
        })
}

/// 用域名构造 SNI（TCP 层连的是 IP，二者解耦——手册 §1.3.1）。
pub fn server_name(host: &str) -> Result<ServerName<'static>, NetworkError> {
    ServerName::try_from(host.to_owned())
        .map_err(|err| NetworkError::CertError(format!("非法 SNI `{host}`：{err}")))
}

/// `--check-certificate=false` 的放行校验器：不断言链，只做握手签名验证。
#[derive(Debug)]
struct NoVerifier(Arc<CryptoProvider>);

impl ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpn_follows_enable_http2() {
        let h2 = client_config(&TlsOptions::default()).expect("构造 h2 配置");
        assert_eq!(
            h2.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );

        let http1 = client_config(&TlsOptions::default().http1_only()).expect("构造 http1 配置");
        assert_eq!(http1.alpn_protocols, vec![b"http/1.1".to_vec()]);
    }

    #[test]
    fn builds_config_with_and_without_verification() {
        let mut options = TlsOptions::default();
        assert!(client_config(&options).is_ok());
        options.check_certificate = false;
        assert!(client_config(&options).is_ok());
    }

    #[test]
    fn sni_accepts_domain_and_rejects_garbage() {
        assert!(server_name("raw.gitcode.com").is_ok());
        assert!(server_name("bad name!").is_err());
    }

    #[test]
    fn private_key_without_certificate_is_rejected() {
        let options = TlsOptions {
            private_key: Some(PathBuf::from("key.pem")),
            ..TlsOptions::default()
        };
        assert!(matches!(
            client_config(&options),
            Err(NetworkError::CertError(_))
        ));
    }

    /// 自签证书 + PEM 私钥：验证 `--certificate` / `--private-key` 路径能构造配置。
    #[test]
    fn loads_client_certificate_from_pem() {
        let dir = tempfile::tempdir().expect("临时目录");
        let certified = rcgen::generate_simple_self_signed(vec!["client.test".into()])
            .expect("生成自签证书");
        let cert_path = dir.path().join("client.pem");
        let key_path = dir.path().join("client.key");
        std::fs::write(&cert_path, certified.cert.pem()).expect("写证书");
        std::fs::write(&key_path, certified.signing_key.serialize_pem()).expect("写私钥");

        let options = TlsOptions {
            check_certificate: false,
            client_certificate: Some(cert_path.clone()),
            private_key: Some(key_path),
            ..TlsOptions::default()
        };
        assert!(client_config(&options).is_ok());

        // 私钥缺省时从证书文件读取：证书文件里没有私钥块 → 明确报错。
        let options = TlsOptions {
            check_certificate: false,
            client_certificate: Some(cert_path),
            private_key: None,
            ..TlsOptions::default()
        };
        assert!(matches!(
            client_config(&options),
            Err(NetworkError::CertError(msg)) if msg.contains("私钥")
        ));
    }
}
