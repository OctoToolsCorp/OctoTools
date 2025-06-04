# OctoTools: Solana Automation Suite

OctoTools is a powerful and comprehensive application designed to automate various operations on the Solana blockchain, with a strong focus on token management, trading, and advanced transaction strategies. It features a modern, intuitive Graphical User Interface (GUI) built with Egui, providing a seamless experience for both novice and experienced users.

## ✨ Features

*   **Modern GUI**: A sleek and responsive user interface built with Egui, offering easy access to all functionalities.
*   **Wallet Management**: Generate, load, and manage multiple Solana wallets, including dedicated parent and developer wallets.
*   **Balance Monitoring**: Real-time display of SOL and token balances across all managed wallets.
*   **Token Launchpad**:
    *   Create and launch new tokens on platforms like Pump.fun and Bonk/Raydium.
    *   Configure token details (name, symbol, description, image, social links).
    *   Automate initial buys from developer and other designated wallets.
    *   Support for Jito bundles for priority transaction inclusion.
*   **Atomic Buy/Sell**: Execute multi-wallet buy and sell orders for tokens with advanced controls for slippage and priority fees.
*   **Volume Bot**: Simulate trading activity with automated buy/sell cycles across multiple wallets, configurable for various strategies.
*   **SOL Dispersal & Gathering**: Easily distribute SOL from a parent wallet to multiple sub-wallets or consolidate SOL back to the parent.
*   **Address Lookup Table (ALT) Management**: Create, manage, and deactivate ALTs to optimize transaction size and reduce fees.
*   **Profit & Loss (P&L) Tracking**: Calculate realized P&L for specific tokens based on transaction history.
*   **WSOL Unwrapping**: Convert Wrapped SOL (WSOL) back to native SOL across all wallets.
*   **Key Generation**: Generate new Solana keypairs and save them to files.
*   **Flexible Configuration**: Customize RPC endpoints, commitment levels, Jito settings, and default transaction parameters.

## 🚀 Getting Started

### Prerequisites

*   **Rust & Cargo**: Ensure you have Rust and Cargo installed. You can download them from [rust-lang.org](https://www.rust-lang.org/tools/install).
*   **Solana CLI Tools (Optional but Recommended)**: For interacting with the Solana blockchain from your terminal. Install from [solana.com](https://docs.solana.com/cli/install-solana-cli).
*   **A Solana Wallet with SOL**: You'll need SOL in your parent wallet for transaction fees and funding other operations.

### Installation

1.  **Clone the Repository**:
    ```bash
    git clone https://github.com/OctoToolsCorp/OctoTools.git
    cd OctoTools
    ```
2.  **Build the Application**:
    ```bash
    cargo build --release
    ```
    This will compile the application and create an executable in the `target/release/` directory.

### Running the GUI

To launch the OctoTools GUI, run the compiled executable:

```bash
cargo run --bin solana-pumpfun-token-gui # Run the GUI application
```

## ⚙️ Configuration

OctoTools uses a `settings.json` file (located in the application's working directory) to store your configurations. You can manage most settings directly from the GUI under the "Settings" view.

### Key Settings Explained:

*   **Solana RPC URL**: The endpoint for connecting to the Solana blockchain (e.g., `https://api.mainnet-beta.solana.com`).
*   **Commitment Level**: Determines the transaction finality you wait for (`processed`, `confirmed`, `finalized`).
*   **Jito Configuration**:
    *   **Block Engine Endpoint**: Select a Jito block engine for MEV-aware transaction submission.
    *   **Jito Tip Account Pubkey**: The address to send Jito tips to.
    *   **Jito Tip Amount (SOL)**: The amount of SOL to tip for Jito bundles.
*   **Wallet & Key Management**:
    *   **Keys File Path**: The JSON file containing your loaded wallets.
    *   **Parent Wallet Private Key**: Your primary wallet's private key (used for funding, gathering, and some operations).
    *   **Dev Wallet Private Key**: A dedicated wallet for token creation/minting.
*   **Default Transaction Parameters**: Set default slippage and priority fees for transactions.

## 🧭 GUI Views Overview

*   **🚀 Launch**: Create and deploy new tokens, configure initial buys, and manage Jito bundles.
*   **💸 Sell**: Execute individual or mass sell orders for tokens, with options for percentage or fixed amounts.
*   **💥 Atomic Buy**: Perform coordinated multi-wallet token purchases in a single bundle.
*   **💨 Disperse**: Distribute SOL from your parent wallet to selected sub-wallets.
*   **📥 Gather**: Consolidate SOL from multiple sub-wallets back to your parent wallet.
*   **🔍 Check**: Monitor SOL and token balances across all wallets and calculate P&L for specific tokens.
*   **🔧 ALT Management**: Tools for creating, managing, and deactivating Solana Address Lookup Tables.
*   **⚙️ Settings**: Configure all application parameters, including RPC, Jito, and wallet settings.
*   **📈 Volume Bot**: Set up and run automated trading simulations with customizable buy/sell logic.

## ⚠️ Security Notes

*   **Private Keys**: Your private keys are sensitive. OctoTools stores them locally in your `settings.json` file. Ensure this file is protected and never share it.
*   **Dedicated Wallets**: Consider using dedicated wallets with minimal funds for automated operations to limit potential exposure.
*   **Transaction Fees**: Always ensure you have sufficient SOL in your parent wallet to cover transaction fees.

## 🤝 Contributing

Contributions are welcome! Please refer to the `CONTRIBUTING.md` file (if available) for guidelines.

## 📄 License

This project is licensed under the MIT License. See the `LICENSE` file for details.