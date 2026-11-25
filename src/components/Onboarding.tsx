// First-run onboarding flow: full-screen wrapper around AddProjectFlow, with a dependency
// step first when herdr is missing or too old. Shown by App.tsx when currentProject()
// returns null (no cancel affordance — first-run requires completing setup). The project
// step machine lives in AddProjectFlow.

import { useEffect, useState } from "react";
import { depsStepVisible, needsDepsStep, stepModel } from "../lib/deps";
import { AddProjectFlow } from "./AddProjectFlow";
import { useDeps } from "./DepsProvider";
import { DepsStep } from "./DepsStep";

interface OnboardingProps {
  /** Called after a project is successfully registered and active. */
  onComplete: () => void;
}

export function Onboarding({ onComplete }: OnboardingProps) {
  const { state } = useDeps();
  // Once a snapshot needs the step it stays until the user moves on, so it doesn't vanish
  // mid-install or the moment herdr turns up. A later snapshot can still turn it on: the
  // first one often lands before the server's version is known.
  const [seen, setSeen] = useState(false);
  const [dismissed, setDismissed] = useState(false);
  const needs = needsDepsStep(state.snapshot);
  useEffect(() => {
    if (needs) setSeen(true);
  }, [needs]);
  const depsStep = depsStepVisible({ loaded: state.loaded, needs, seen, dismissed });

  if (depsStep === null) {
    return <div className="msg-card">Loading…</div>;
  }
  // A herdr picked with Browse fixes things without a run to report; go straight on.
  if (depsStep && stepModel(state.snapshot, state.progress).kind !== "hidden") {
    return <DepsStep onDone={() => setDismissed(true)} />;
  }
  // First-run: no cancel — the user must add a project to proceed.
  return <AddProjectFlow onComplete={onComplete} />;
}
