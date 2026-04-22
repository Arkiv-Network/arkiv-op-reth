use arkiv_genesis::{generate_genesis, GenesisConfig};

fn main() -> eyre::Result<()> {
    let config = GenesisConfig::default();
    let genesis = generate_genesis(&config)?;
    let json = serde_json::to_string_pretty(&genesis)?;
    println!("{json}");
    Ok(())
}
