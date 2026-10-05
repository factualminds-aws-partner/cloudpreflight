# aws-ecommerce-plan

A Terraform plan in JSON form (`terraform show -json`) for a storefront that is being
modernised: a legacy EC2 instance and its data volume are removed, the database is
resized and made Multi-AZ, the web service doubles its task count, and a cache, CDN,
order-events function and cart table are added.

    cloudpreflight scan fixtures/aws-ecommerce-plan
    cloudpreflight estimate fixtures/aws-ecommerce-plan/tfplan.json --format json

The plan is hand-written for the test suite; it contains no real account data.
