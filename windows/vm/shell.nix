{ pkgs ? import <nixpkgs> { } }:

let
  python = pkgs.python3;
  extraAnsibleDeps = pypkgs: [
    pypkgs.libvirt
    pypkgs.lxml
  ];
in
pkgs.mkShell rec {
  buildInputs = [ ansible ];

  LC_ALL = "C.UTF-8";

  ansible = python.pkgs.toPythonApplication
    (python.pkgs.ansible-core.overridePythonAttrs (old: {
      dependencies = (old.dependencies or []) ++ extraAnsibleDeps python.pkgs;
    }));

  ansiblePython = python.withPackages extraAnsibleDeps;
  ANSIBLE_PYTHON_INTERPRETER = ansiblePython + "/bin/python";
}
