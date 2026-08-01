// Test query for the StackableTrinoODBC connector.
// Evaluate this file in the Power Query SDK (VS Code) to test the connector.
//
// Targets the repository's own stack, which `./integration-tests/setup.sh`
// starts. That coordinator serves HTTPS on 8443 and nothing else, and its
// certificate is signed by the CA `scripts/gen-certs.sh` generates, which no
// machine trusts, hence TlsVerify. Point elsewhere by editing the arguments; a
// plaintext coordinator takes 8080 and "http".
//
// `protocol` is passed rather than left out: omitting it selects "https", the
// same default the driver applies.
//
// The password is not here. Set a UsernamePassword credential in the SDK and
// it supplies both the user and the password, overriding the "admin" below,
// which is only read under anonymous authentication. The stack's credentials
// are admin/admin.
//
// Navigation is two steps because the connector sets
// HierarchicalNavigation = true: schema, then table. The Schema argument sets
// the session default for unqualified names and does not collapse that.
let
    result = StackableTrinoODBC.Contents(
        "localhost", 8443, "tpcds", "sf1", "admin", "https",
        [TlsVerify = "false"]
    ),
    schema = result{[Name = "sf1"]}[Data],
    customer = schema{[Name = "customer"]}[Data]
in
    Table.FirstN(customer, 5)
