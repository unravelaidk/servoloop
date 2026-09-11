# Security policy

ServoLoop controls systems that can affect the physical world. Treat every
model response, tool argument, sensor reading, and network message as untrusted.

## Safety requirements

Before connecting ServoLoop to a physical robot, you must provide independent
hardware emergency stops, workspace limits, collision protection, and a tested
manual recovery procedure. Don't rely on an LLM, this library, process
isolation, or a software stop as the only safety layer.

Run new models, prompts, drivers, and policies in simulation first. Record
commands and observations, test boundary conditions, and require human approval
for operations whose failures can cause harm.

## Report a vulnerability

Don't open a public issue for an undisclosed vulnerability. Use GitHub's private
vulnerability reporting feature on this repository and include reproduction
steps, affected versions, and potential impact.
