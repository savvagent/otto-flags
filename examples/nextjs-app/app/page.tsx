import { FeatureDemo } from './components/FeatureDemo';
import { FlagOverridePanel } from './components/FlagOverridePanel';

/**
 * Main Page Component
 * Uses client components with @otto-flags/nextjs SDK
 */
export default function Home() {
  return (
    <main>
      <FeatureDemo />
      <FlagOverridePanel />
    </main>
  );
}
