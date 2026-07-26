// Test query for the StackableTrinoODBC connector.
// Evaluate this file in the Power Query SDK (VS Code) to test the connector.
let
    result = StackableTrinoODBC.Contents("localhost", 8080, "tpcds"),
    schema = result{[Name = "sf1"]}[Data],
    customer = schema{[Name = "customer"]}[Data]
in
    Table.FirstN(customer, 5)
