# Example storefront: CloudFront + ALB + ECS on Fargate + RDS + ElastiCache + S3,
# with an order-events Lambda and a DynamoDB cart table.

terraform {
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 6.0"
    }
  }
}

provider "aws" {
  region = var.region
}

variable "region" {
  default = "us-east-1"
}

variable "environment" {
  default = "production"
}

variable "web_task_count" {
  default = 4
}

locals {
  name = "storefront-${var.environment}"
  azs  = ["us-east-1a", "us-east-1b"]
}

# --- Network -----------------------------------------------------------------

resource "aws_vpc" "main" {
  cidr_block = "10.0.0.0/16"
}

resource "aws_subnet" "public" {
  count             = length(local.azs)
  vpc_id            = aws_vpc.main.id
  cidr_block        = "10.0.${count.index}.0/24"
  availability_zone = local.azs[count.index]
}

resource "aws_subnet" "private" {
  count             = length(local.azs)
  vpc_id            = aws_vpc.main.id
  cidr_block        = "10.0.${count.index + 10}.0/24"
  availability_zone = local.azs[count.index]
}

resource "aws_eip" "nat" {
  count  = length(local.azs)
  domain = "vpc"
}

resource "aws_nat_gateway" "main" {
  count         = length(local.azs)
  allocation_id = aws_eip.nat[count.index].id
  subnet_id     = aws_subnet.public[count.index].id
}

resource "aws_security_group" "web" {
  name   = "${local.name}-web"
  vpc_id = aws_vpc.main.id
}

# --- Edge and load balancing ---------------------------------------------------

resource "aws_lb" "web" {
  name               = "${local.name}-web"
  load_balancer_type = "application"
  subnets            = aws_subnet.public[*].id
  security_groups    = [aws_security_group.web.id]
}

resource "aws_lb_target_group" "web" {
  name        = "${local.name}-web"
  port        = 8080
  protocol    = "HTTP"
  target_type = "ip"
  vpc_id      = aws_vpc.main.id
}

resource "aws_cloudfront_distribution" "storefront" {
  enabled = true

  origin {
    domain_name = aws_lb.web.dns_name
    origin_id   = "alb"
  }

  default_cache_behavior {
    target_origin_id       = "alb"
    viewer_protocol_policy = "redirect-to-https"
    allowed_methods        = ["GET", "HEAD"]
    cached_methods         = ["GET", "HEAD"]
  }

  restrictions {
    geo_restriction {
      restriction_type = "none"
    }
  }

  viewer_certificate {
    cloudfront_default_certificate = true
  }
}

# --- Application ---------------------------------------------------------------

resource "aws_ecs_cluster" "main" {
  name = local.name
}

resource "aws_ecs_task_definition" "web" {
  family                   = "${local.name}-web"
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = 1024
  memory                   = 2048
  container_definitions = jsonencode([{
    name  = "web"
    image = "public.ecr.aws/nginx/nginx:stable"
  }])
}

resource "aws_ecs_service" "web" {
  name            = "web"
  cluster         = aws_ecs_cluster.main.id
  task_definition = aws_ecs_task_definition.web.arn
  launch_type     = "FARGATE"
  desired_count   = var.web_task_count

  network_configuration {
    subnets         = aws_subnet.private[*].id
    security_groups = [aws_security_group.web.id]
  }
}

resource "aws_cloudwatch_log_group" "web" {
  name              = "/ecs/${local.name}/web"
  retention_in_days = 30
}

# --- Data ----------------------------------------------------------------------

resource "aws_db_instance" "orders" {
  identifier        = "${local.name}-orders"
  engine            = "postgres"
  instance_class    = "db.m6g.large"
  allocated_storage = 200
  storage_type      = "gp3"
  multi_az          = true
  username          = "storefront"
  password          = "change-me-in-secrets-manager"
}

resource "aws_elasticache_replication_group" "sessions" {
  replication_group_id = "${local.name}-sessions"
  description          = "Session and catalog cache"
  engine               = "redis"
  node_type            = "cache.m6g.large"
  num_cache_clusters   = 2
}

resource "aws_s3_bucket" "catalog" {
  bucket = "${local.name}-catalog-assets"
}

resource "aws_s3_bucket" "logs" {
  bucket = "${local.name}-logs"
}

resource "aws_s3_bucket_lifecycle_configuration" "logs" {
  bucket = aws_s3_bucket.logs.id

  rule {
    id     = "expire"
    status = "Enabled"
    expiration {
      days = 90
    }
  }
}

resource "aws_dynamodb_table" "carts" {
  name         = "${local.name}-carts"
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "cart_id"

  attribute {
    name = "cart_id"
    type = "S"
  }
}

# --- Background jobs -----------------------------------------------------------

resource "aws_iam_role" "order_events" {
  name               = "${local.name}-order-events"
  assume_role_policy = jsonencode({ Version = "2012-10-17", Statement = [] })
}

resource "aws_lambda_function" "order_events" {
  function_name = "${local.name}-order-events"
  role          = aws_iam_role.order_events.arn
  runtime       = "python3.13"
  handler       = "app.handler"
  filename      = "order_events.zip"
  memory_size   = 512
  architectures = ["arm64"]
}
