# Benchmark cluster for Aeron Cluster with the Rust engine: three nodes and a load client.
# Every instance terminates itself after var.lifetime_hours, even if nobody runs destroy.

terraform {
  required_version = ">= 1.6"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
}

variable "region" {
  type    = string
  default = "us-east-1"
}

variable "ami" {
  description = "Debian 13 for x86_64, pinned to match the IAM policy. The binaries need its glibc."
  type        = string
  default     = "ami-0412e376e50785f2a"
}

variable "instance_type" {
  type    = string
  default = "c6id.2xlarge"
}

variable "lifetime_hours" {
  description = "Instances shut down and terminate after this long."
  type        = number
  default     = 4
}

variable "ssh_public_key_path" {
  type    = string
  default = "~/.ssh/id_ed25519.pub"
}

variable "allow_egress" {
  description = "Open while hosts install packages, then closed for the benchmark."
  type        = bool
  default     = true
}

variable "ssh_allowed_cidr" {
  description = "The only address allowed to SSH in."
  type        = string
}

provider "aws" {
  region = var.region
  default_tags {
    tags = { Project = "mmx-cluster-bench" }
  }
}

data "aws_ec2_instance_type_offerings" "available" {
  location_type = "availability-zone"
  filter {
    name   = "instance-type"
    values = [var.instance_type]
  }
}

locals {
  zone = sort(data.aws_ec2_instance_type_offerings.available.locations)[0]
  hosts = {
    node0  = "10.77.1.10"
    node1  = "10.77.1.11"
    node2  = "10.77.1.12"
    client = "10.77.1.13"
  }
}

resource "aws_vpc" "bench" {
  cidr_block           = "10.77.0.0/16"
  enable_dns_hostnames = true
}

resource "aws_internet_gateway" "bench" {
  vpc_id = aws_vpc.bench.id
}

resource "aws_subnet" "bench" {
  vpc_id                  = aws_vpc.bench.id
  cidr_block              = "10.77.1.0/24"
  availability_zone       = local.zone
  map_public_ip_on_launch = true
}

resource "aws_route_table" "bench" {
  vpc_id = aws_vpc.bench.id
  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.bench.id
  }
}

resource "aws_route_table_association" "bench" {
  subnet_id      = aws_subnet.bench.id
  route_table_id = aws_route_table.bench.id
}

resource "aws_security_group" "bench" {
  name_prefix = "mmx-bench-"
  vpc_id      = aws_vpc.bench.id

  ingress {
    description = "SSH from the operator only"
    from_port   = 22
    to_port     = 22
    protocol    = "tcp"
    cidr_blocks = [var.ssh_allowed_cidr]
  }

  ingress {
    description = "Aeron and everything else between bench hosts"
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    self        = true
  }

  egress {
    description = "Between bench hosts"
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    self        = true
  }

  dynamic "egress" {
    for_each = var.allow_egress ? [1] : []
    content {
      description = "Package installs"
      from_port   = 0
      to_port     = 0
      protocol    = "-1"
      cidr_blocks = ["0.0.0.0/0"]
    }
  }
}

# Keeps the hosts close together, for the lowest network latency EC2 offers.
resource "aws_placement_group" "bench" {
  name     = "mmx-bench"
  strategy = "cluster"
}

resource "aws_key_pair" "bench" {
  key_name_prefix = "mmx-bench-"
  public_key      = file(pathexpand(var.ssh_public_key_path))
}

resource "aws_instance" "host" {
  for_each = local.hosts

  ami                                  = var.ami
  instance_type                        = var.instance_type
  subnet_id                            = aws_subnet.bench.id
  private_ip                           = each.value
  vpc_security_group_ids               = [aws_security_group.bench.id]
  placement_group                      = aws_placement_group.bench.id
  key_name                             = aws_key_pair.bench.key_name
  instance_initiated_shutdown_behavior = "terminate"
  user_data_replace_on_change          = true

  user_data = templatefile("${path.module}/cloud-init.sh", {
    lifetime_minutes = var.lifetime_hours * 60
  })

  root_block_device {
    volume_size = 16
    volume_type = "gp3"
  }

  metadata_options {
    http_tokens = "required"
  }

  tags = { Name = "mmx-bench-${each.key}" }

  depends_on = [aws_route_table_association.bench, aws_internet_gateway.bench]
}

output "public_ips" {
  value = { for name, host in aws_instance.host : name => host.public_ip }
}

output "private_ips" {
  value = local.hosts
}

output "zone" {
  value = local.zone
}
