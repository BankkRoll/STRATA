/**
 * Renders a component with test services (fixture-backed, with the given
 * capabilities) and fake feature services.
 */
import { render } from "@testing-library/react";
import type { ReactNode } from "react";
import { FeaturesContext } from "../features";
import { ServicesContext, type Services } from "../services";
import { fakeFeatures, type FeatureOverrides } from "./features";
import { testServices } from "./services";

/**
 * @param caps - Backend command names the build "has".
 * @param overrides - Working feature calls.
 * @param ui - Component under test.
 * @param services - Base services.
 */
export function renderFeature(caps: string[], overrides: FeatureOverrides, ui: ReactNode, services: Services = testServices()) {
  const s: Services = { ...services, capabilities: () => new Set(caps) };
  return render(
    <ServicesContext value={s}>
      <FeaturesContext value={fakeFeatures(overrides)}>{ui}</FeaturesContext>
    </ServicesContext>,
  );
}
