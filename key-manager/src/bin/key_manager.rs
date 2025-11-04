use aws_config::SdkConfig;
use aws_sdk_s3::{
    config::Region as S3Region, operation::put_object::PutObjectOutput, Client as S3Client,
    Error as S3Error,
};
use aws_sdk_secretsmanager::{
    operation::{get_secret_value::GetSecretValueOutput, put_secret_value::PutSecretValueOutput},
    Client as SecretsManagerClient, Error as SecretsManagerError,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use clap::{Parser, Subcommand};
use eyre::Result;
use rand::{thread_rng, Rng};
use reqwest::Client;
use sodiumoxide::crypto::box_::{curve25519xsalsa20poly1305, PublicKey, SecretKey, Seed};

const PUBLIC_KEY_S3_BUCKET_NAME: &str = "wf-smpcv2-stage-public-keys";
const PUBLIC_KEY_S3_OBJECT_PREFIX: &str = "public-key";

#[derive(Debug, Parser)] // requires `derive` feature
#[command(name = "key-manager")]
#[command(about = "Key manager CLI", long_about = None)]
struct KeyManagerCli {
    #[command(subcommand)]
    command: Commands,

    #[arg(
        short, long, env, value_parser = clap::builder::PossibleValuesParser::new(& ["0", "1", "2"])
    )]
    node_id: String,

    #[arg(long, env, default_value = "stage")]
    env: String,

    #[arg(short, long, env, default_value = "eu-north-1")]
    region: String,

    #[arg(long, env, default_value = None)]
    endpoint_url: Option<String>,

    #[arg(short, long, env, default_value = "iris-mpc")]
    app_name: String,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Rotate private and public keys
    #[command(arg_required_else_help = true)]
    Rotate {
        #[arg(short, long, env)]
        dry_run: Option<bool>,

        #[arg(short, long, env)]
        public_key_bucket_name: Option<String>,
    },
    /// Validate private key in key manager against public keys (either provided
    /// or in s3)
    Validate {
        // AWSCURRENT or AWSPREVIOUS or a specific version
        #[arg(
            short, long, env, value_parser = clap::builder::PossibleValuesParser::new(& ["AWSCURRENT", "AWSPREVIOUS"])
        )]
        version_stage: String,

        #[arg(short, long, env)]
        b64_pub_key: Option<String>,

        #[arg(short, long, env)]
        public_key_bucket_name: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = KeyManagerCli::parse();
    let region = args.region;

    let region_provider = S3Region::new(region.clone());
    let shared_config = aws_config::from_env().region(region_provider).load().await;

    let bucket_key_name = format!("{}-{}", PUBLIC_KEY_S3_OBJECT_PREFIX, args.node_id);
    let private_key_secret_id: String = format!(
        "{}/{}/ecdh-private-key-{}",
        args.env, args.app_name, args.node_id
    );

    match args.command {
        Commands::Rotate {
            dry_run,
            public_key_bucket_name,
        } => {
            rotate_keys(
                &shared_config,
                &bucket_key_name,
                &private_key_secret_id,
                dry_run,
                public_key_bucket_name,
                args.endpoint_url,
            )
            .await?;
        }
        Commands::Validate {
            version_stage,
            b64_pub_key,
            public_key_bucket_name,
        } => {
            validate_keys(
                &shared_config,
                &private_key_secret_id,
                &version_stage,
                b64_pub_key,
                &bucket_key_name,
                public_key_bucket_name,
                region.clone(),
                args.endpoint_url,
            )
            .await?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn validate_keys(
    sdk_config: &SdkConfig,
    secret_id: &str,
    version_stage: &str,
    b64_pub_key: Option<String>,
    bucket_key_name: &str,
    public_key_bucket_name: Option<String>,
    region: String,
    endpoint_url: Option<String>,
) -> Result<()> {
    let mut sm_config_builder = aws_sdk_secretsmanager::config::Builder::from(sdk_config);

    if let Some(endpoint_url) = endpoint_url.as_ref() {
        sm_config_builder = sm_config_builder.endpoint_url(endpoint_url);
    }

    let sm_client = SecretsManagerClient::from_conf(sm_config_builder.build());

    let bucket_name = if let Some(bucket_name) = public_key_bucket_name {
        bucket_name
    } else {
        PUBLIC_KEY_S3_BUCKET_NAME.to_string()
    };
    // Parse user-provided public key, if present
    let pub_key = if let Some(b64_pub_key) = b64_pub_key {
        let user_pubkey = STANDARD.decode(b64_pub_key.as_bytes()).unwrap();
        match PublicKey::from_slice(&user_pubkey) {
            Some(key) => key,
            None => panic!("Invalid public key"),
        }
    } else {
        // Otherwise, get the latest one from S3 using HTTPS
        let user_pubkey_string =
            download_key_from_s3(bucket_name.as_str(), bucket_key_name, region.clone()).await?;
        let user_pubkey = STANDARD.decode(user_pubkey_string.as_bytes()).unwrap();
        match PublicKey::from_slice(&user_pubkey) {
            Some(key) => key,
            None => panic!("Invalid public key"),
        }
    };

    let private_key = download_key_from_asm(&sm_client, secret_id, version_stage).await?;
    let data = private_key.secret_string.unwrap();
    let user_privkey = STANDARD.decode(data.as_bytes()).unwrap();
    let decoded_priv_key = SecretKey::from_slice(&user_privkey).unwrap();

    assert_eq!(decoded_priv_key.public_key(), pub_key);
    Ok(())
}

async fn rotate_keys(
    sdk_config: &SdkConfig,
    bucket_key_name: &str,
    private_key_secret_id: &str,
    dry_run: Option<bool>,
    public_key_bucket_name: Option<String>,
    endpoint_url: Option<String>,
) -> Result<()> {
    let mut rng = thread_rng();

    let bucket_name = if let Some(bucket_name) = public_key_bucket_name {
        bucket_name
    } else {
        PUBLIC_KEY_S3_BUCKET_NAME.to_string()
    };

    let mut seedbuf = [0u8; 32];
    rng.fill(&mut seedbuf);
    let pk_seed = Seed(seedbuf);

    let mut s3_config_builder = aws_sdk_s3::config::Builder::from(sdk_config);
    let mut sm_config_builder = aws_sdk_secretsmanager::config::Builder::from(sdk_config);

    if let Some(endpoint_url) = endpoint_url.as_ref() {
        s3_config_builder = s3_config_builder.endpoint_url(endpoint_url);
        s3_config_builder = s3_config_builder.force_path_style(true);
        sm_config_builder = sm_config_builder.endpoint_url(endpoint_url);
    }

    let s3_client = S3Client::from_conf(s3_config_builder.build());
    let sm_client = SecretsManagerClient::from_conf(sm_config_builder.build());

    let (public_key, private_key) = generate_key_pairs(pk_seed);
    let pub_key_str = STANDARD.encode(public_key);
    let priv_key_str = STANDARD.encode(private_key.clone());

    if dry_run.unwrap_or(false) {
        println!("Dry run enabled, skipping upload of public key to S3");
        println!("Public key: {}", pub_key_str);

        let user_pubkey = STANDARD.decode(pub_key_str.as_bytes()).unwrap();
        let decoded_pub_key = PublicKey::from_slice(&user_pubkey).unwrap();

        assert_eq!(public_key, decoded_pub_key);

        let user_privkey = STANDARD.decode(priv_key_str.as_bytes()).unwrap();
        let decoded_priv_key = SecretKey::from_slice(&user_privkey).unwrap();

        assert_eq!(private_key, decoded_priv_key);

        return Ok(());
    }
    match upload_public_key_to_s3(
        &s3_client,
        bucket_name.as_str(),
        bucket_key_name,
        pub_key_str.as_str(),
    )
    .await
    {
        Ok(output) => {
            println!("Bucket: {}", bucket_name);
            println!("Key: {}", bucket_key_name);
            println!("ETag: {}", output.e_tag.unwrap());
        }
        Err(e) => {
            eprintln!("Error uploading public key to S3: {:?}", e);
            return Err(eyre::eyre!("Error uploading public key to S3"));
        }
    }

    match upload_private_key_to_asm(&sm_client, private_key_secret_id, priv_key_str.as_str()).await
    {
        Ok(output) => {
            println!("Secret ARN: {}", output.arn.unwrap());
            println!("Secret Name: {}", output.name.unwrap());
            println!("Version ID: {}", output.version_id.unwrap());
        }
        Err(e) => {
            eprintln!("Error uploading private key to Secrets Manager: {:?}", e);
            return Err(eyre::eyre!(
                "Error uploading private key to Secrets Manager"
            ));
        }
    }

    println!("File uploaded successfully!");

    Ok(())
}

async fn download_key_from_s3(
    bucket: &str,
    key: &str,
    region: String,
) -> Result<String, reqwest::Error> {
    print!("Downloading key from S3 bucket: {} key: {}", bucket, key);
    let s3_url = format!("https://{}.s3.{}.amazonaws.com/{}", bucket, region, key);
    let client = Client::new();
    let response = client.get(&s3_url).send().await?.text().await?;
    Ok(response)
}

async fn download_key_from_asm(
    client: &SecretsManagerClient,
    secret_id: &str,
    version_stage: &str,
) -> Result<GetSecretValueOutput, SecretsManagerError> {
    Ok(client
        .get_secret_value()
        .secret_id(secret_id)
        .version_stage(version_stage)
        .send()
        .await?)
}

async fn upload_private_key_to_asm(
    client: &SecretsManagerClient,
    secret_id: &str,
    content: &str,
) -> Result<PutSecretValueOutput, SecretsManagerError> {
    Ok(client
        .put_secret_value()
        .secret_string(content)
        .secret_id(secret_id)
        .send()
        .await?)
}

async fn upload_public_key_to_s3(
    client: &S3Client,
    bucket: &str,
    key: &str,
    content: &str,
) -> Result<PutObjectOutput, S3Error> {
    Ok(client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(content.to_string().into_bytes().into())
        .send()
        .await?)
}

fn generate_key_pairs(seed: Seed) -> (PublicKey, SecretKey) {
    // Generate an ephemeral secret (private key)
    let (public_key, private_key) = curve25519xsalsa20poly1305::keypair_from_seed(&seed);

    (public_key, private_key)
}

// tests
#[cfg(test)]
mod test {
    use super::*;
    use sodiumoxide::crypto::sealedbox;

    pub fn get_public_key(user_pub_key: &str) -> PublicKey {
        let user_pubkey = STANDARD.decode(user_pub_key.as_bytes()).unwrap();
        match PublicKey::from_slice(&user_pubkey) {
            Some(key) => key,
            None => panic!("Invalid public key"),
        }
    }

    #[test]
    fn test_encode_pk_to_pem() {
        let (public_key, _) = generate_key_pairs(Seed([0u8; 32]));
        let pub_key_str = STANDARD.encode(public_key);
        let decoded_pub_key = get_public_key(&pub_key_str);
        assert_eq!(public_key, decoded_pub_key);
    }

    #[test]
    fn test_encode_and_decode_shares() {
        let (server_public_key, server_private_key) = generate_key_pairs(Seed([0u8; 32]));
        let share = "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";
        let ciphertext = sealedbox::seal(share.as_bytes(), &server_public_key);

        let server_iris_code_plaintext =
            sealedbox::open(&ciphertext, &server_public_key, &server_private_key).unwrap();

        assert_eq!(share.as_bytes(), server_iris_code_plaintext.as_slice());
    }
}
