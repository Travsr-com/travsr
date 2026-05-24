variable "tenancy_ocid" {
  description = "OCID of the OCI tenancy. Found under Profile → Tenancy in the OCI Console."
  type        = string
}

variable "user_ocid" {
  description = "OCID of the OCI user used for API authentication. Found under Profile → My Profile in the OCI Console."
  type        = string
}

variable "fingerprint" {
  description = "Fingerprint of the API signing key associated with the user. Found under Profile → My Profile → API Keys in the OCI Console."
  type        = string
}

variable "private_key_path" {
  description = "Absolute path to the PEM-encoded private API signing key on the operator's local machine."
  type        = string
}

variable "region" {
  description = "OCI region identifier. Defaults to ap-mumbai-1 (Always Free home region)."
  type        = string
  default     = "ap-mumbai-1"
}

variable "compartment_ocid" {
  description = "OCID of the compartment in which all resources will be created. Use the root compartment OCID or a dedicated travsr compartment."
  type        = string
}

variable "ssh_public_key" {
  description = "Contents of the SSH public key (e.g., contents of ~/.ssh/id_ed25519.pub) to be injected into the instance for operator access."
  type        = string
}

variable "instance_display_name" {
  description = "Display name for the MCP server compute instance in the OCI Console."
  type        = string
  default     = "travsr-mcp-server"
}

variable "vcn_cidr" {
  description = "CIDR block for the Virtual Cloud Network."
  type        = string
  default     = "10.0.0.0/16"
}

variable "subnet_cidr" {
  description = "CIDR block for the public subnet within the VCN."
  type        = string
  default     = "10.0.1.0/24"
}

variable "block_volume_size_gb" {
  description = "Size of the block volume in GB. Defaults to 50 GB (Always Free tier limit). WARNING: Reducing this value after first apply is a destructive operation — OCI does not support shrinking block volumes. Increasing requires a manual backup/restore cycle if the volume is already formatted."
  type        = number
  default     = 50
}

variable "ocir_namespace" {
  description = "OCI Container Registry namespace (object storage namespace) for the tenancy. Found in OCI Console → Developer Services → Container Registry → top of the page."
  type        = string
}

variable "ocir_repo_name" {
  description = "Name of the OCIR repository for the travsr-mcp Docker image."
  type        = string
  default     = "travsr-mcp"
}

variable "availability_domain" {
  description = "Availability domain for the compute instance and block volume. Must be in the selected region. Example: 'tUwm:AP-MUMBAI-1-AD-1'. Found in OCI Console → Compute → Instances → Create Instance → Placement."
  type        = string
}

variable "image_ocid" {
  description = "OCID of the compute image to use. Must be Ubuntu 22.04 Minimal for ARM64 (aarch64) in the operator's chosen region. Navigate to OCI Console → Compute → Images → Platform Images, filter by Ubuntu 22.04 Minimal and Shape compatibility VM.Standard.A1.Flex to find the correct OCID for your region."
  type        = string
}
