use soroban_sdk::contracttype;

#[contracttype]
pub enum DataKey {
    /// Fee in basis points (e.g. 30 = 0.3%)
    FeeBps,
    /// Address that receives protocol fees
    FeeRecipient,
    /// Minimum allowed stream duration in seconds
    MinDurationSeconds,
    /// Maximum rate per second in stroops
    MaxRatePerSecond,
    /// The DripFactory contract this governor controls
    FactoryAddress,
    /// Multisig / authority address allowed to change parameters
    Authority,
}
