use sha2::{Sha256, Digest};
use hex; // Added for hex encoding

// Define the same secret salt used in the validation logic.
// IMPORTANT: This must match the salt in gui_main.rs
const LICENSE_SALT: &str = "a7Gz!pX3qR#sW2vY";

fn main() {
    // Get the hardware ID from the user
    println!("Enter the hardware ID displayed in the application (e.g., ABCD-EFGH-IJKL-MNOP):");
    let mut hardware_id_input = String::new();
    std::io::stdin().read_line(&mut hardware_id_input).expect("Failed to read line");
    let hardware_id_input = hardware_id_input.trim();
    
    // Remove hyphens from hardware ID and convert to lowercase
    let clean_hardware_id_temp: String = hardware_id_input.chars().filter(|c| *c != '-').collect();
    let clean_hardware_id = clean_hardware_id_temp.to_lowercase(); // Ensure lowercase
    println!("Clean hardware ID for processing (lowercase): {}", clean_hardware_id);
    
    if clean_hardware_id.len() < 8 { // Ensure the base ID is long enough
        println!("ERROR: Cleaned hardware ID is too short (must be at least 8 characters). Cannot generate key.");
        return;
    }

    // --- Enhanced Key Prefix Derivation ---

    // 1. salted_hw_id_hash = SHA256(lowercase_clean_hw_id + LICENSE_SALT_BYTES)
    let mut hasher1 = Sha256::new();
    hasher1.update(clean_hardware_id.as_bytes());
    hasher1.update(LICENSE_SALT.as_bytes());
    let salted_hw_id_hash_full = hasher1.finalize();
    println!("1. Salted HW ID Hash (Full): {:?}", salted_hw_id_hash_full);

    // 2. hw_id_prefix_bytes = first_8_bytes(lowercase_clean_hw_id)
    let hw_id_prefix_bytes: Vec<u8> = clean_hardware_id.as_bytes().iter().take(8).cloned().collect();
    if hw_id_prefix_bytes.len() < 8 {
        println!("ERROR: Hardware ID (clean) too short to get 8-byte prefix.");
        return;
    }
    println!("2. HW ID Prefix Bytes: {:?}", hw_id_prefix_bytes);

    // 3. salted_hash_prefix_bytes = first_8_bytes(salted_hw_id_hash)
    let salted_hash_prefix_bytes: Vec<u8> = salted_hw_id_hash_full.iter().take(8).cloned().collect();
    println!("3. Salted HW ID Hash Prefix Bytes: {:?}", salted_hash_prefix_bytes);

    // 4. key_material = hw_id_prefix_bytes + salted_hash_prefix_bytes
    let mut key_material = Vec::with_capacity(16);
    key_material.extend_from_slice(&hw_id_prefix_bytes);
    key_material.extend_from_slice(&salted_hash_prefix_bytes);
    println!("4. Key Material (HW_ID_prefix + Salted_Hash_prefix): {:?}", key_material);

    // 5. final_key_prefix_bytes = SHA256(key_material)[0..7]
    let mut hasher2 = Sha256::new();
    hasher2.update(&key_material);
    let final_hash_full = hasher2.finalize();
    let final_key_prefix_bytes: Vec<u8> = final_hash_full.iter().take(8).cloned().collect();
    println!("5. Final Key Prefix Bytes (Hashed Key Material Prefix): {:?}", final_key_prefix_bytes);
    
    // 6. license_key_hex_prefix = hex_encode_upper(final_key_prefix_bytes)
    let key_prefix_hex = hex::encode_upper(&final_key_prefix_bytes);
    println!("6. License Key Hex Prefix (Encoded Step 5): {}", key_prefix_hex);

    // The license key will now be the 16-char hex prefix + a short suffix.
    // Example: HEXHEXHEXHEXHEXHEXHEXHEX-SUFF
    // Total length before hyphens: 16 (hex) + 4 (suffix) = 20 chars.
    let suffix = "ABCD"; // Example suffix, can be anything or empty
    let full_key_string = format!("{}{}", key_prefix_hex, suffix);
    
    // Format for easy reading (e.g., XXXX-XXXX-XXXX-XXXX-XXXX)
    let mut formatted_key = String::new();
    for (i, char_val) in full_key_string.chars().enumerate() {
        if i > 0 && i % 4 == 0 {
            formatted_key.push('-');
        }
        formatted_key.push(char_val);
    }
    
    println!("\nGenerated license key (HEX encoded prefix):");
    println!("Hardware ID used for generation (cleaned): {}", clean_hardware_id);
    println!("Salt used: {}", LICENSE_SALT);
    // println!("Key XOR part (raw bytes): {:?}", key_xor_part_bytes); // No longer direct XOR part
    println!("Final Key Prefix (Hex Encoded for key): {}", key_prefix_hex);
    println!("┌{:─^52}┐", ""); // Adjusted width for longer key
    println!("│{:^52}│", formatted_key);
    println!("└{:─^52}┘", "");
    println!("\nNote: The first 16 characters of this key (when non-hyphenated) are the hex representation of the crucial 8 bytes.");
    println!("The validator will decode these 16 hex characters back to 8 bytes for the XOR check.");
    println!("The suffix '{}' is for length and appearance.", suffix);

    println!("\nPress Enter to exit...");
    let mut temp_input = String::new();
    std::io::stdin().read_line(&mut temp_input).expect("Failed to read line");
}